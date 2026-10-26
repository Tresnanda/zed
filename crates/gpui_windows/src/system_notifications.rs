//! System notifications as Windows toast notifications.

use std::cell::RefCell;
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash as _, Hasher as _};
use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    ForegroundExecutor, SharedString, SystemNotification, SystemNotificationResponse, Task,
};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager, ToastNotifier,
};
use windows::core::{IInspectable, Interface as _, h};

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

/// The toast a tag last showed, kept only so `dismiss` can hide it. The serial
/// tells a toast's own activated/dismissed event from a later post's.
type ActiveToasts = Rc<RefCell<HashMap<SharedString, (u64, ToastNotification)>>>;

const MAX_ACTIVE_TOASTS: usize = 64;

/// What the toast event handlers (WinRT threads) tell the foreground task.
enum ToastEvent {
    Response(SystemNotificationResponse),
    /// The toast was clicked, which removes it from the action center: nothing is left to hide.
    Gone {
        tag: SharedString,
        serial: u64,
    },
}

pub(crate) struct SystemNotificationState {
    notifier: Option<ToastNotifier>,
    active_toasts: ActiveToasts,
    next_serial: u64,
    response_sender: mpsc::UnboundedSender<ToastEvent>,
    response_receiver: Option<mpsc::UnboundedReceiver<ToastEvent>>,
    callback: ResponseCallback,
    _response_task: Option<Task<()>>,
}

impl SystemNotificationState {
    pub(crate) fn new() -> Self {
        let (response_sender, response_receiver) = mpsc::unbounded();
        Self {
            notifier: None,
            active_toasts: Rc::new(RefCell::new(HashMap::new())),
            next_serial: 0,
            response_sender,
            response_receiver: Some(response_receiver),
            callback: Rc::new(RefCell::new(None)),
            _response_task: None,
        }
    }

    pub(crate) fn show(
        &mut self,
        has_package_identity: bool,
        app_identity: Option<(&str, &str)>,
        notification: SystemNotification,
    ) -> windows::core::Result<()> {
        let Some(notifier) = self.notifier(has_package_identity, app_identity)? else {
            return Ok(());
        };

        let document = toast_document(&notification)?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        // Windows caps toast tags at 64 characters (post-Creators Update),
        // so hash the arbitrary GPUI tag down to a fixed-width value.
        let tag = {
            let mut hasher = DefaultHasher::new();
            notification.tag.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };
        toast.SetTag(&tag.into())?;

        self.next_serial += 1;
        let serial = self.next_serial;
        let sender = self.response_sender.clone();
        let response_tag = notification.tag.clone();
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_sender, arguments| {
                let action_id = arguments
                    .as_ref()
                    .and_then(|arguments| arguments.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|arguments| arguments.Arguments().ok())
                    .filter(|arguments| !arguments.is_empty())
                    .map(|arguments| SharedString::from(arguments.to_string()));
                sender
                    .unbounded_send(ToastEvent::Response(SystemNotificationResponse {
                        tag: response_tag.clone(),
                        action_id,
                    }))
                    .ok();
                sender
                    .unbounded_send(ToastEvent::Gone {
                        tag: response_tag.clone(),
                        serial,
                    })
                    .ok();
                Ok(())
            },
        ))?;
        // No `Hide` of the previous toast: a toast with the same tag replaces
        // it in the action center. Hiding it first failed once the user had
        // dismissed or cleared it, and that error stopped the new toast from
        // showing at all.
        notifier.Show(&toast)?;
        let mut toasts = self.active_toasts.borrow_mut();
        toasts.insert(notification.tag, (serial, toast));
        // Only a click removes a toast from the action center by itself; one the
        // user swiped away stays there, so it stays here to be hidden by `dismiss`.
        // Bound what an app with many tags keeps.
        if toasts.len() > MAX_ACTIVE_TOASTS {
            if let Some(oldest) = toasts
                .iter()
                .min_by_key(|(_, (serial, _))| *serial)
                .map(|(tag, _)| tag.clone())
            {
                toasts.remove(&oldest);
            }
        }
        Ok(())
    }

    pub(crate) fn dismiss(&mut self, tag: &str) {
        let Some((_, toast)) = self.active_toasts.borrow_mut().remove(tag) else {
            return;
        };
        let Some(notifier) = &self.notifier else {
            return;
        };
        if let Err(error) = notifier.Hide(&toast) {
            log::warn!("failed to dismiss system notification: {error}");
        }
    }

    pub(crate) fn on_response(
        &mut self,
        executor: &ForegroundExecutor,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.callback.borrow_mut() = Some(callback);

        if let Some(mut receiver) = self.response_receiver.take() {
            let callback = self.callback.clone();
            let active_toasts = self.active_toasts.clone();
            self._response_task = Some(executor.spawn(async move {
                while let Some(event) = receiver.next().await {
                    let response = match event {
                        ToastEvent::Response(response) => response,
                        ToastEvent::Gone { tag, serial } => {
                            let mut toasts = active_toasts.borrow_mut();
                            if toasts
                                .get(&tag)
                                .is_some_and(|(current, _)| *current == serial)
                            {
                                toasts.remove(&tag);
                            }
                            continue;
                        }
                    };
                    // Take the callback out for the call: it may re-enter the
                    // platform (e.g. to dismiss the notification it was told
                    // about) or replace itself.
                    let taken = callback.borrow_mut().take();
                    if let Some(mut taken) = taken {
                        taken(response);
                        callback.borrow_mut().get_or_insert(taken);
                    }
                }
            }));
        }
    }

    fn notifier(
        &mut self,
        has_package_identity: bool,
        app_identity: Option<(&str, &str)>,
    ) -> windows::core::Result<Option<ToastNotifier>> {
        if let Some(notifier) = &self.notifier {
            return Ok(Some(notifier.clone()));
        }

        let notifier = if has_package_identity {
            ToastNotificationManager::CreateToastNotifier()?
        } else {
            let Some((app_identifier, app_name)) = app_identity else {
                log::warn!(
                    "cannot show a system notification without an app identity; \
                     call `App::set_app_identity` during startup"
                );
                return Ok(None);
            };
            register_app_user_model_id(app_identifier, app_name);
            ToastNotificationManager::CreateToastNotifierWithId(&app_identifier.into())?
        };

        self.notifier = Some(notifier.clone());
        Ok(Some(notifier))
    }
}

fn toast_document(notification: &SystemNotification) -> windows::core::Result<XmlDocument> {
    let document = XmlDocument::new()?;
    let toast = document.CreateElement(h!("toast"))?;
    document.AppendChild(&toast)?;

    let visual = document.CreateElement(h!("visual"))?;
    toast.AppendChild(&visual)?;
    let binding = document.CreateElement(h!("binding"))?;
    binding.SetAttribute(h!("template"), h!("ToastGeneric"))?;
    visual.AppendChild(&binding)?;
    for text in [&notification.title, &notification.body] {
        let element = document.CreateElement(h!("text"))?;
        element.SetInnerText(&text.as_ref().into())?;
        binding.AppendChild(&element)?;
    }

    if !notification.actions.is_empty() {
        let actions = document.CreateElement(h!("actions"))?;
        toast.AppendChild(&actions)?;
        for action in &notification.actions {
            let action_element = document.CreateElement(h!("action"))?;
            action_element.SetAttribute(h!("content"), &action.label.as_ref().into())?;
            action_element.SetAttribute(h!("arguments"), &action.id.as_ref().into())?;
            actions.AppendChild(&action_element)?;
        }
    }

    // No <audio> element: Windows plays its default notification sound, like
    // the macOS notification Kybern posts, and Focus Assist silences it. A
    // `silent` post (a replacement the user just heard) turns the sound off.
    if notification.silent {
        let audio = document.CreateElement(h!("audio"))?;
        audio.SetAttribute(h!("silent"), h!("true"))?;
        toast.AppendChild(&audio)?;
    }
    Ok(document)
}

/// Registers the app's AUMID so toasts display correctly for an unpackaged app without a Start Menu shortcut.
fn register_app_user_model_id(app_identifier: &str, app_name: &str) {
    let result = windows_registry::CURRENT_USER
        .create(format!(r"Software\Classes\AppUserModelId\{app_identifier}"))
        .and_then(|key| key.set_string("DisplayName", app_name));
    if let Err(error) = result {
        log::warn!("failed to register AppUserModelID; notifications may not display: {error}");
    }
}
