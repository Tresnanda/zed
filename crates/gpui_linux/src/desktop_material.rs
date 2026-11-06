/// Whether the connected display can composite alpha and blur behind a window.
/// This is a startup probe; no window or GPU is created.
pub fn desktop_material_capabilities() -> (bool, bool) {
    #[cfg(feature = "wayland")]
    if gpui::guess_compositor() == "Wayland" {
        if let Ok(result) = wayland() {
            return result;
        }
    }
    #[cfg(feature = "x11")]
    if let Ok(result) = x11() {
        return result;
    }
    (false, false)
}

#[cfg(feature = "wayland")]
fn wayland() -> anyhow::Result<(bool, bool)> {
    use wayland_client::{
        Connection, Dispatch, QueueHandle, WEnum,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };
    use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
        self, ExtBackgroundEffectManagerV1,
    };
    use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;
    #[derive(Default)]
    struct Probe {
        blur: bool,
    }
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ExtBackgroundEffectManagerV1, ()> for Probe {
        fn event(
            state: &mut Self,
            _: &ExtBackgroundEffectManagerV1,
            event: ext_background_effect_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let ext_background_effect_manager_v1::Event::Capabilities {
                flags: WEnum::Value(flags),
            } = event
            {
                state.blur = flags.contains(ext_background_effect_manager_v1::Capability::Blur);
            }
        }
    }
    wayland_client::delegate_noop!(Probe: ignore OrgKdeKwinBlurManager);
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&connection)?;
    anyhow::ensure!(
        globals
            .contents()
            .with_list(|list| ["wl_compositor", "wl_shm", "xdg_wm_base"]
                .iter()
                .all(|name| list.iter().any(|global| global.interface == *name))),
        "Wayland display lacks required window globals"
    );
    let handle = queue.handle();
    let kde = globals
        .bind::<OrgKdeKwinBlurManager, _, _>(&handle, 1..=1, ())
        .ok();
    let effect = globals
        .bind::<ExtBackgroundEffectManagerV1, _, _>(&handle, 1..=1, ())
        .ok();
    let mut probe = Probe::default();
    if effect.is_some() {
        queue.roundtrip(&mut probe)?;
    }
    let hyprland = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|value| {
            value
                .split(':')
                .any(|part| part.eq_ignore_ascii_case("Hyprland"))
        });
    Ok((true, kde.is_some() || probe.blur || hyprland))
}

#[cfg(feature = "x11")]
fn x11() -> anyhow::Result<(bool, bool)> {
    use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
    let (connection, screen) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen].root;
    let selection = connection
        .intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes())?
        .reply()?
        .atom;
    if connection.get_selection_owner(selection)?.reply()?.owner == 0 {
        return Ok((false, false));
    }
    let blur = connection
        .intern_atom(true, b"_KDE_NET_WM_BLUR_BEHIND_REGION")?
        .reply()?
        .atom;
    if blur == 0 {
        return Ok((true, false));
    }
    // KWin advertises effects as root-window properties, not EWMH _NET_SUPPORTED.
    // This matches KWindowEffectsPrivateX11::isEffectAvailable.
    let properties = connection.list_properties(root)?.reply()?.atoms;
    Ok((true, properties.contains(&blur)))
}

// Match the addon's 12 logical-pixel CSD clip. Interior rows form one rectangle;
// only the corner bands need individual scanlines.
pub(crate) fn rounded_region(width: i32, height: i32, radius: i32) -> Vec<[i32; 4]> {
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    let radius = radius.max(0).min(width / 2).min(height / 2);
    if radius == 0 {
        return vec![[0, 0, width, height]];
    }
    let mut rectangles = vec![[0, radius, width, height - radius * 2]];
    for y in 0..radius {
        let dy = f64::from(radius - y) - 0.5;
        let inset =
            (f64::from(radius) - (f64::from(radius * radius) - dy * dy).sqrt()).ceil() as i32;
        rectangles.push([inset, y, width - 2 * inset, 1]);
        rectangles.push([inset, height - y - 1, width - 2 * inset, 1]);
    }
    rectangles
}
