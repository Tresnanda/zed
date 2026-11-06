#![cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

pub use linux::current_platform;

mod desktop_material;
pub use desktop_material::desktop_material_capabilities;
