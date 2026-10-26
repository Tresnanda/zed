//! System notifications over the XDG notifications D-Bus interface.
//!
//! One worker thread owns one session-bus connection for the life of the
//! process and sends every `Notify` and `CloseNotification` on it, in order. A
//! second thread listens on the same connection for `ActionInvoked`,
//! `NotificationClosed` and the notification service changing owner, and keeps
//! the table of which server-side notification each tag last produced. A post
//! for a tag replaces its notification (`replaces_id`); a notification the
//! server closed, or a server that restarted, empties its table entry, so a
//! stale id never replaces another application's notification.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    ForegroundExecutor, SharedString, SystemNotification, SystemNotificationResponse, Task,
};
use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator, connection::Builder, fdo::DBusProxy};
use zbus::message::Type as MessageType;
use zbus::zvariant::Value;

use std::cell::RefCell;
use std::rc::Rc;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const BUS_SERVICE: &str = "org.freedesktop.DBus";

/// The XDG action key invoked when the user activates the notification body
/// rather than a specific action button.
const DEFAULT_ACTION: &str = "default";
/// A server that does not answer a call within this long is treated as absent
/// for that call; the worker moves on to the next command.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The server-side notification a tag last produced, and what its action keys
/// mean.
#[derive(Debug, PartialEq)]
struct Slot {
    server_id: u32,
    /// The transport key each action button was given → the caller's id.
    actions: HashMap<String, SharedString>,
}

/// The notifications of this process the server still shows, by tag.
#[derive(Default)]
struct Slots {
    by_tag: HashMap<SharedString, Slot>,
}

impl Slots {
    /// The server id a post under `tag` replaces, or 0 for a new notification.
    fn replaces(&self, tag: &str) -> u32 {
        self.by_tag.get(tag).map_or(0, |slot| slot.server_id)
    }

    fn insert(&mut self, tag: SharedString, slot: Slot) {
        self.by_tag.insert(tag, slot);
    }

    fn remove_tag(&mut self, tag: &str) -> Option<Slot> {
        self.by_tag.remove(tag)
    }

    /// Forgets the notification the server reported closed, whoever closed it.
    fn closed(&mut self, server_id: u32) {
        self.by_tag.retain(|_, slot| slot.server_id != server_id);
    }

    /// The server went away or was replaced: nothing it showed can be replaced.
    fn clear(&mut self) {
        self.by_tag.clear();
    }

    /// Resolves a click on `server_id` to the tag and the pressed action's id
    /// (`None` for the body). A server closes a notification it activated, so
    /// the slot is released: the next post under the tag is a new notification.
    fn activated(
        &mut self,
        server_id: u32,
        key: &str,
    ) -> Option<(SharedString, Option<SharedString>)> {
        let tag = self
            .by_tag
            .iter()
            .find_map(|(tag, slot)| (slot.server_id == server_id).then(|| tag.clone()))?;
        let action_id = {
            let slot = self.by_tag.get(&tag)?;
            response_action_id(key, &slot.actions)
        };
        let Some(action_id) = action_id else {
            log::warn!("system notification returned unknown action {key:?}");
            return None;
        };
        self.by_tag.remove(&tag);
        Some((tag, action_id))
    }
}

type SharedSlots = Arc<Mutex<Slots>>;

fn lock(slots: &SharedSlots) -> std::sync::MutexGuard<'_, Slots> {
    slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

/// What the UI thread asks the worker to do.
enum Command {
    Show(Post),
    Dismiss(SharedString),
}

struct Post {
    /// `(identifier, name)` from `set_app_identity`.
    identity: Option<(String, String)>,
    notification: SystemNotification,
}

/// The connection's threads. `alive` goes false when either ends, and the next
/// post then starts a fresh service (the session bus or the app's connection
/// to it went away).
struct Service {
    commands: std_mpsc::Sender<Command>,
    alive: Arc<AtomicBool>,
}

pub(crate) struct SystemNotificationState {
    response_sender: mpsc::UnboundedSender<SystemNotificationResponse>,
    response_receiver: Option<mpsc::UnboundedReceiver<SystemNotificationResponse>>,
    callback: ResponseCallback,
    _response_task: Option<Task<()>>,
    slots: SharedSlots,
    service: Mutex<Option<Service>>,
}

impl SystemNotificationState {
    pub(crate) fn new() -> Self {
        let (response_sender, response_receiver) = mpsc::unbounded();
        Self {
            response_sender,
            response_receiver: Some(response_receiver),
            callback: Rc::new(RefCell::new(None)),
            _response_task: None,
            slots: Arc::new(Mutex::new(Slots::default())),
            service: Mutex::new(None),
        }
    }

    /// `identity` is the `(identifier, name)` given to `set_app_identity`. The
    /// identifier doubles as the desktop entry and the icon name, which is how
    /// an installed app (`<identifier>.desktop`, an icon of the same name)
    /// shows its own icon and groups its notifications.
    pub(crate) fn show(&self, identity: Option<(&str, &str)>, notification: SystemNotification) {
        self.send(Command::Show(Post {
            identity: identity.map(|(identifier, name)| (identifier.to_string(), name.to_string())),
            notification,
        }));
    }

    /// Closes the tag's notification on the server, if it is still there.
    pub(crate) fn dismiss(&self, tag: &str) {
        self.send(Command::Dismiss(SharedString::from(tag.to_string())));
    }

    fn send(&self, command: Command) {
        let Ok(mut service) = self.service.lock() else {
            return;
        };
        if service
            .as_ref()
            .is_none_or(|service| !service.alive.load(Ordering::Acquire))
        {
            *service = Service::start(self.slots.clone(), self.response_sender.clone());
        }
        if let Some(service) = service.as_ref() {
            service.commands.send(command).ok();
        }
    }

    pub(crate) fn on_response(
        &mut self,
        executor: &ForegroundExecutor,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.callback.borrow_mut() = Some(callback);

        // Responses arrive from the listener thread; this task hands them to
        // the registered callback on the main thread.
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

impl Service {
    /// Starts the worker, which connects off the UI thread. No session bus (a
    /// headless session) is an ordinary setup, not a fault.
    fn start(
        slots: SharedSlots,
        responses: mpsc::UnboundedSender<SystemNotificationResponse>,
    ) -> Option<Self> {
        let (commands, receiver) = std_mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = alive.clone();
        let spawned = std::thread::Builder::new()
            .name("system-notifications".to_string())
            .spawn(move || {
                let connection = match connect() {
                    Ok(connection) => connection,
                    Err(error) => {
                        log::debug!("no session bus for system notifications: {error}");
                        worker_alive.store(false, Ordering::Release);
                        return;
                    }
                };
                if let Err(error) =
                    listen(&connection, slots.clone(), responses, worker_alive.clone())
                {
                    log::debug!("cannot listen for notification signals: {error}");
                    worker_alive.store(false, Ordering::Release);
                    return;
                }
                for command in receiver {
                    if !worker_alive.load(Ordering::Acquire) {
                        break;
                    }
                    match command {
                        Command::Show(post) => {
                            if let Err(error) = notify(&connection, &slots, post) {
                                // No notification daemon is an ordinary setup.
                                log::debug!("failed to show system notification: {error}");
                            }
                        }
                        Command::Dismiss(tag) => {
                            let Some(slot) = lock(&slots).remove_tag(&tag) else {
                                continue;
                            };
                            if let Err(error) = connection.call_method(
                                Some(SERVICE),
                                PATH,
                                Some(SERVICE),
                                "CloseNotification",
                                &(slot.server_id,),
                            ) {
                                log::debug!("failed to close system notification: {error}");
                            }
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self { commands, alive }),
            Err(error) => {
                log::warn!("failed to spawn the system notification thread: {error}");
                None
            }
        }
    }
}

fn connect() -> zbus::Result<Connection> {
    Builder::session()?.method_timeout(CALL_TIMEOUT).build()
}

/// Subscribes the connection to the notification service's signals and to its
/// name changing owner, and starts the thread that reads them.
fn listen(
    connection: &Connection,
    slots: SharedSlots,
    responses: mpsc::UnboundedSender<SystemNotificationResponse>,
    alive: Arc<AtomicBool>,
) -> zbus::Result<()> {
    let bus = DBusProxy::new(connection)?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(SERVICE)?
            .interface(SERVICE)?
            .path(PATH)?
            .build(),
    )?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(BUS_SERVICE)?
            .interface(BUS_SERVICE)?
            .member("NameOwnerChanged")?
            .arg(0, SERVICE)?
            .build(),
    )?;
    let messages = MessageIterator::from(connection);
    let spawned = std::thread::Builder::new()
        .name("system-notification-signals".to_string())
        .spawn(move || {
            for message in messages {
                let Ok(message) = message else {
                    continue;
                };
                let header = message.header();
                if header.message_type() != MessageType::Signal {
                    continue;
                }
                let (Some(interface), Some(member)) = (header.interface(), header.member()) else {
                    continue;
                };
                match (interface.as_str(), member.as_str()) {
                    (SERVICE, "ActionInvoked") => {
                        let Ok((server_id, key)) = message.body().deserialize::<(u32, String)>()
                        else {
                            continue;
                        };
                        let resolved = lock(&slots).activated(server_id, &key);
                        if let Some((tag, action_id)) = resolved {
                            responses
                                .unbounded_send(SystemNotificationResponse { tag, action_id })
                                .ok();
                        }
                    }
                    (SERVICE, "NotificationClosed") => {
                        if let Ok((server_id, _reason)) = message.body().deserialize::<(u32, u32)>()
                        {
                            lock(&slots).closed(server_id);
                        }
                    }
                    (BUS_SERVICE, "NameOwnerChanged") => {
                        if let Ok((name, _old, _new)) =
                            message.body().deserialize::<(String, String, String)>()
                        {
                            if name == SERVICE {
                                log::debug!("the notification service changed owner");
                                lock(&slots).clear();
                            }
                        }
                    }
                    _ => {}
                }
            }
            // The connection closed: the next post starts a new service.
            alive.store(false, Ordering::Release);
        });
    if let Err(error) = spawned {
        log::warn!("failed to spawn the notification signal thread: {error}");
        return Err(zbus::Error::Failure(error.to_string()));
    }
    Ok(())
}

/// One `Notify` call. Runs on the worker, so two posts for a tag are sent in
/// order and the second carries the first one's server id.
fn notify(connection: &Connection, slots: &SharedSlots, post: Post) -> zbus::Result<()> {
    let Post {
        identity,
        notification,
    } = post;
    let (app_name, icon) = match &identity {
        Some((identifier, name)) => (name.as_str(), identifier.as_str()),
        None => ("", ""),
    };
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    if let Some((identifier, _)) = &identity {
        hints.insert("desktop-entry", Value::from(identifier.as_str()));
    }
    if notification.silent {
        hints.insert("suppress-sound", Value::from(true));
    } else {
        hints.insert("sound-name", Value::from("message-new-instant"));
    }
    let mut action_keys: Vec<(String, &str)> = vec![(DEFAULT_ACTION.to_string(), DEFAULT_ACTION)];
    let mut actions = HashMap::new();
    for (index, action) in notification.actions.iter().enumerate() {
        let transport_id = format!("gpui-action-{index}");
        action_keys.push((transport_id.clone(), action.label.as_ref()));
        actions.insert(transport_id, action.id.clone());
    }
    let action_list: Vec<&str> = action_keys
        .iter()
        .flat_map(|(key, label)| [key.as_str(), *label])
        .collect();

    let replaces_id = lock(slots).replaces(&notification.tag);
    let reply = connection.call_method(
        Some(SERVICE),
        PATH,
        Some(SERVICE),
        "Notify",
        &(
            app_name,
            replaces_id,
            icon,
            notification.title.as_ref(),
            notification.body.as_ref(),
            action_list,
            hints,
            -1i32,
        ),
    )?;
    let server_id: u32 = reply.body().deserialize()?;
    lock(slots).insert(notification.tag, Slot { server_id, actions });
    Ok(())
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

    fn slot(server_id: u32) -> Slot {
        Slot {
            server_id,
            actions: HashMap::new(),
        }
    }

    #[test]
    fn a_post_replaces_the_notification_its_tag_last_produced() {
        let mut slots = Slots::default();
        assert_eq!(slots.replaces("thread-1"), 0);
        slots.insert("thread-1".into(), slot(7));
        slots.insert("thread-2".into(), slot(8));
        assert_eq!(slots.replaces("thread-1"), 7);
        assert_eq!(slots.replaces("thread-2"), 8);
        // The server hands the replacement the same id, or a new one.
        slots.insert("thread-1".into(), slot(9));
        assert_eq!(slots.replaces("thread-1"), 9);
    }

    #[test]
    fn a_closed_notification_or_a_restarted_server_forgets_its_ids() {
        let mut slots = Slots::default();
        slots.insert("thread-1".into(), slot(7));
        slots.insert("thread-2".into(), slot(8));
        slots.closed(7);
        assert_eq!(slots.replaces("thread-1"), 0);
        assert_eq!(slots.replaces("thread-2"), 8);
        slots.clear();
        assert_eq!(slots.replaces("thread-2"), 0);
        // An unknown id is ignored.
        slots.closed(99);
    }

    #[test]
    fn a_click_resolves_to_the_tag_once_and_frees_the_slot() {
        let mut slots = Slots::default();
        slots.insert(
            "thread-1".into(),
            Slot {
                server_id: 7,
                actions: HashMap::from([("gpui-action-0".to_string(), SharedString::from("open"))]),
            },
        );
        assert_eq!(
            slots.activated(7, DEFAULT_ACTION),
            Some(("thread-1".into(), None))
        );
        // A second click on the same server id finds nothing: one click, one report.
        assert_eq!(slots.activated(7, DEFAULT_ACTION), None);
        assert_eq!(slots.replaces("thread-1"), 0);

        slots.insert(
            "thread-1".into(),
            Slot {
                server_id: 8,
                actions: HashMap::from([("gpui-action-0".to_string(), SharedString::from("open"))]),
            },
        );
        assert_eq!(slots.activated(8, "unknown"), None);
        assert_eq!(
            slots.replaces("thread-1"),
            8,
            "an unknown key keeps the slot"
        );
        assert_eq!(
            slots.activated(8, "gpui-action-0"),
            Some(("thread-1".into(), Some("open".into())))
        );
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

    /// Against a real bus and the test server of `apps/gpuix/native/notify-server.py` (python3-gi), not run unless
    /// `KYBERN_NOTIFY_TEST_LOG` and `KYBERN_NOTIFY_TEST_CONTROL` name its log and control files:
    ///
    /// ```sh
    /// dbus-run-session -- sh -c 'python3 notify-server.py --log /tmp/n.log --control /tmp/n.ctl & sleep 2;
    ///   KYBERN_NOTIFY_TEST_LOG=/tmp/n.log KYBERN_NOTIFY_TEST_CONTROL=/tmp/n.ctl cargo test -p gpui_linux live_server'
    /// ```
    #[test]
    fn live_server() {
        let (Ok(log), Ok(control)) = (
            std::env::var("KYBERN_NOTIFY_TEST_LOG"),
            std::env::var("KYBERN_NOTIFY_TEST_CONTROL"),
        ) else {
            return;
        };
        let entries = |op: &str| -> Vec<String> {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(&format!("\"op\": \"{op}\"")))
                .map(str::to_string)
                .collect()
        };
        let number = |line: &str, key: &str| -> u32 {
            let marker = format!("\"{key}\": ");
            let start = line.find(&marker).unwrap() + marker.len();
            line[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap()
        };
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            for _ in 0..200 {
                if done() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!(
                "timed out waiting for {what}: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        };
        let command = |line: &str| {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&control)
                .unwrap();
            writeln!(file, "{line}").unwrap();
        };
        let post = |state: &SystemNotificationState, tag: &str, silent: bool| {
            state.show(
                Some(("dev.kybern.gpuix.test", "Kybern")),
                SystemNotification {
                    tag: tag.to_string().into(),
                    title: "Title".into(),
                    body: "Body".into(),
                    actions: Vec::new(),
                    silent,
                },
            );
        };
        let mut state = SystemNotificationState::new();
        let mut responses = state.response_receiver.take().unwrap();

        // A first post, then a replacement of it.
        post(&state, "t1", false);
        wait("the first Notify", &|| entries("Notify").len() >= 1);
        post(&state, "t1", true);
        wait("the second Notify", &|| entries("Notify").len() >= 2);
        let notify = entries("Notify");
        assert_eq!(number(&notify[0], "replaces_id"), 0);
        assert!(notify[0].contains("sound-name"), "{}", notify[0]);
        assert_eq!(number(&notify[1], "replaces_id"), number(&notify[0], "id"));
        assert!(
            notify[1].contains("suppress-sound") && !notify[1].contains("sound-name"),
            "{}",
            notify[1]
        );
        assert!(notify[0].contains("desktop-entry") && notify[0].contains("dev.kybern.gpuix.test"));

        // Two posts back to back for one tag: the second still carries the first one's id.
        post(&state, "t2", false);
        post(&state, "t2", true);
        wait("both t2 Notify calls", &|| entries("Notify").len() >= 4);
        let notify = entries("Notify");
        assert_eq!(number(&notify[2], "replaces_id"), 0);
        assert_eq!(number(&notify[3], "replaces_id"), number(&notify[2], "id"));
        let t2_id = number(&notify[3], "id");

        // A click resolves to the tag once.
        command(&format!("invoke {t2_id} default"));
        let response = futures::executor::block_on(async {
            use futures::StreamExt as _;
            responses.next().await
        })
        .unwrap();
        assert_eq!(response.tag.as_ref(), "t2");
        assert_eq!(response.action_id, None);
        // The click freed the slot: the next post is new.
        post(&state, "t2", false);
        wait("the post after the click", &|| entries("Notify").len() >= 5);
        assert_eq!(number(&entries("Notify")[4], "replaces_id"), 0);

        // Dismiss closes the server-side notification and frees the tag.
        let t1_id = number(&entries("Notify")[1], "id");
        state.dismiss("t1");
        wait("CloseNotification", &|| !entries("Close").is_empty());
        assert_eq!(number(&entries("Close")[0], "id"), t1_id);
        post(&state, "t1", false);
        wait("the post after the dismiss", &|| {
            entries("Notify").len() >= 6
        });
        assert_eq!(number(&entries("Notify")[5], "replaces_id"), 0);

        // A restarted server numbers from 1 again: a stale replaces_id would replace another app's notification.
        let before = entries("Notify").len();
        command("restart");
        wait("the server to come back", &|| entries("ready").len() >= 2);
        std::thread::sleep(Duration::from_millis(300));
        post(&state, "t1", false);
        wait("the post after the restart", &|| {
            entries("Notify").len() > before
        });
        let notify = entries("Notify");
        assert_eq!(
            number(&notify[before], "replaces_id"),
            0,
            "{}",
            notify[before]
        );
    }
}
