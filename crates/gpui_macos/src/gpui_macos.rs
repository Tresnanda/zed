#![cfg(target_os = "macos")]
//! macOS platform implementation for GPUI.
//!
//! macOS screens have a y axis that goes up from the bottom of the screen and
//! an origin at the bottom left of the main display.

mod dispatcher;
mod display;
mod display_link;
mod events;
mod keyboard;
mod pasteboard;
mod system_notifications;

#[cfg(feature = "screen-capture")]
mod screen_capture;

use gpui_apple::metal_renderer as renderer;

pub mod metal_renderer {
    pub use gpui_apple::metal_renderer::{
        InstanceBufferPool, MetalRenderer, PathRasterizationVertex, PathSprite, SurfaceBounds,
    };

    #[cfg(any(test, feature = "test-support"))]
    pub use gpui_apple::metal_renderer::MetalHeadlessRenderer;
}

#[cfg(feature = "font-kit")]
mod open_type;

#[cfg(feature = "font-kit")]
mod text_system;

mod platform;
mod window;
mod window_appearance;

use cocoa::{
    base::{id, nil},
    foundation::{NSAutoreleasePool, NSNotFound, NSString, NSUInteger},
};

use objc::runtime::{BOOL, NO, YES};
use std::{
    ffi::{CStr, c_char},
    ops::Range,
};

pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use display_link::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use window::*;

#[cfg(feature = "font-kit")]
pub use text_system::MacTextSystem;

pub use display_link::set_frame_wake_hook;
pub use platform::MacPlatform;
pub use window::{FrameWatchdogReport, frame_watchdog_report};
static WINDOW_STATE_OBSERVER: std::sync::RwLock<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = std::sync::RwLock::new(None);
/// Install a nonblocking notification for window occlusion, key, move, screen,
/// appearance and accessibility-display changes (embedded hosts re-read state).
pub fn set_window_state_observer(observer: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
    if observer.is_some() {
        static ACCESSIBILITY: std::sync::Once = std::sync::Once::new();
        // Reduce transparency, increase contrast and reduce motion change in
        // System Settings while this app may not be key. Registered once; the
        // block lives as long as the process.
        ACCESSIBILITY.call_once(|| unsafe {
            use cocoa::{base::{id, nil}, foundation::NSString};
            use objc::{class, msg_send, sel, sel_impl};
            let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let center: id = msg_send![workspace, notificationCenter];
            let name = NSString::alloc(nil).init_str("NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification");
            let block = block::ConcreteBlock::new(|_: id| notify_window_state()).copy();
            let _: id = msg_send![center, addObserverForName: name object: nil queue: nil usingBlock: &*block];
            std::mem::forget(block);
        });
    }
    *WINDOW_STATE_OBSERVER.write().unwrap_or_else(std::sync::PoisonError::into_inner) = observer;
}
pub(crate) fn notify_window_state() {
    let observer = WINDOW_STATE_OBSERVER.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    if let Some(observer) = observer { observer(); }
}

trait BoolExt {
    fn to_objc(self) -> BOOL;
}

impl BoolExt for bool {
    fn to_objc(self) -> BOOL {
        if self { YES } else { NO }
    }
}

trait NSStringExt {
    unsafe fn to_str(&self) -> &str;
}

impl NSStringExt for id {
    unsafe fn to_str(&self) -> &str {
        unsafe {
            let cstr = self.UTF8String();
            if cstr.is_null() {
                ""
            } else {
                CStr::from_ptr(cstr as *mut c_char).to_str().unwrap()
            }
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NSRange {
    pub location: NSUInteger,
    pub length: NSUInteger,
}

impl NSRange {
    fn invalid() -> Self {
        Self {
            location: NSNotFound as NSUInteger,
            length: 0,
        }
    }

    fn is_valid(&self) -> bool {
        self.location != NSNotFound as NSUInteger
    }

    fn to_range(self) -> Option<Range<usize>> {
        if self.is_valid() {
            let start = self.location as usize;
            let end = start + self.length as usize;
            Some(start..end)
        } else {
            None
        }
    }
}

impl From<Range<usize>> for NSRange {
    fn from(range: Range<usize>) -> Self {
        NSRange {
            location: range.start as NSUInteger,
            length: range.len() as NSUInteger,
        }
    }
}

unsafe impl objc::Encode for NSRange {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{NSRange={}{}}}",
            NSUInteger::encode().as_str(),
            NSUInteger::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// Allow NSString::alloc use here because it sets autorelease
#[allow(clippy::disallowed_methods)]
unsafe fn ns_string(string: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(string).autorelease() }
}
