use anyhow::anyhow;
use log::info;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

mod protocol {
    #![allow(clippy::all)]

    use wayland_client;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        wayland_scanner::generate_interfaces!("vendor/gamescope-control.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("vendor/gamescope-control.xml");
}

use protocol::gamescope_control;

const MIN_VERSION: u32 = 2;

struct AppData {
    control: Option<gamescope_control::GamescopeControl>,
    is_internal_display: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppData {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "gamescope_control" && version >= MIN_VERSION {
                info!("found gamescope_control v{version} (global {name})");
                state.control = Some(registry.bind::<gamescope_control::GamescopeControl, _, _>(
                    name,
                    version.min(gamescope_control::EVT_APP_PERFORMANCE_STATS_SINCE),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<gamescope_control::GamescopeControl, ()> for AppData {
    fn event(
        state: &mut Self,
        _: &gamescope_control::GamescopeControl,
        event: gamescope_control::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let gamescope_control::Event::ActiveDisplayInfo { display_flags, .. } = event {
            let is_internal = matches!(
                display_flags,
                WEnum::Value(flags) if flags.contains(gamescope_control::DisplayFlag::InternalDisplay)
            );
            state.is_internal_display = is_internal;
            info!(
                "active display: {}",
                if is_internal { "internal" } else { "external" }
            );
        }
    }
}

pub struct GamescopeConnection {
    conn: Connection,
    queue: wayland_client::EventQueue<AppData>,
    control: gamescope_control::GamescopeControl,
    state: AppData,
}

impl GamescopeConnection {
    pub fn try_connect() -> Option<Self> {
        // copying Connection::connect_to_env but with a different name
        let name: std::path::PathBuf = std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY")
            .unwrap_or_else(|| "gamescope-0".into())
            .into();
        let socket_path = if name.is_absolute() {
            name
        } else {
            let mut p: std::path::PathBuf = std::env::var_os("XDG_RUNTIME_DIR")?.into();
            if !p.is_absolute() {
                return None;
            }
            p.push(name);
            p
        };
        let stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
        let conn = Connection::from_socket(stream).ok()?;
        let display = conn.display();
        let mut state = AppData {
            control: None,
            is_internal_display: false,
        };
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = display.get_registry(&qh, ());

        // First roundtrip: receive globals, send bind.
        // Second roundtrip: receive active_display_info sent in response to bind.
        queue.roundtrip(&mut state).ok()?;
        queue.roundtrip(&mut state).ok()?;

        let control = state.control.take()?;

        info!("gamescope connection established");
        Some(Self {
            conn,
            queue,
            control,
            state,
        })
    }

    pub fn set_fps(&mut self, fps: u16) -> anyhow::Result<()> {
        let _ = self.queue.dispatch_pending(&mut self.state);

        let mut flags = gamescope_control::TargetRefreshCycleFlag::empty();
        if self.state.is_internal_display {
            flags |= gamescope_control::TargetRefreshCycleFlag::InternalDisplay;
        }
        self.control.set_app_target_refresh_cycle(fps as u32, flags);
        self.conn.flush().map_err(|e| anyhow!(e))?;
        Ok(())
    }
}
