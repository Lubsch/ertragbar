use std::io::BufRead;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::thread::JoinHandle;

use nanoserde::DeJson;
use rustix::{event, mm};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

use crate::render::{GlyphCache, Layout};
use crate::{
    battery::Battery, network::Network, render::LOGICAL_HEIGHT, tags::TagState, time::Time,
};

mod battery;
mod network;
mod render;
mod socket;
mod tags;
mod time;

#[derive(Debug)]
struct Bar {
    network: Network,
    battery: Battery,
    time: Time,
}

struct Monitor {
    name: String,
    dirty: bool,
    tags: [TagState; 9],
    wl_output: WlOutput,
    surface: Option<WlSurface>,
    viewport: Option<WpViewport>,
    layout: Layout,
}

struct App {
    compositor: Option<WlCompositor>,
    layer_shell: Option<ZwlrLayerShellV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    shm: Option<WlShm>,
    buffers: Option<Buffers>,

    monitors: Vec<Monitor>,
    glyph_cache: GlyphCache,
    bar: Bar,
    dirty: bool,
    should_exit: bool,
}

/// A memfd-backed pool holding exactly two buffers we alternate between.
/// `busy` marks one the compositor still holds.
struct Buffers {
    _file: OwnedFd,
    pool: WlShmPool,
    /// mmap covering both buffers; valid for the life of this struct.
    map: *mut u8,
    map_len: usize,
    width: i32,
    height: i32,
    slots: [Slot; 2],
}

struct Slot {
    buffer: WlBuffer,
    offset: usize,
    busy: bool,
}

impl Buffers {
    fn new(shm: &WlShm, qh: &QueueHandle<App>, width: i32, height: i32) -> anyhow::Result<Self> {
        let stride = width as usize * 4;
        let one = stride * height as usize;
        let map_len = one * 2;

        let file = rustix::fs::memfd_create("ertragbar", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&file, map_len as u64)?;

        // SAFETY: file is a fresh memfd of exactly map_len bytes.
        let map = unsafe {
            mm::mmap(
                std::ptr::null_mut(),
                map_len,
                mm::ProtFlags::READ | mm::ProtFlags::WRITE,
                mm::MapFlags::SHARED,
                &file,
                0,
            )?
        };

        let pool = shm.create_pool(file.as_fd(), map_len as i32, qh, ());
        let mk = |idx: usize, offset: usize| Slot {
            buffer: pool.create_buffer(
                offset as i32,
                width,
                height,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                idx, // user-data: slot index, so release can clear `busy`
            ),
            offset,
            busy: false,
        };
        let slots = [mk(0, 0), mk(1, one)];

        Ok(Buffers {
            _file: file,
            pool,
            map: map as *mut u8,
            map_len,
            width,
            height,
            slots,
        })
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.busy)
    }

    /// Pixel view of one slot. The slot must not be busy.
    fn pixels(&mut self, idx: usize) -> &mut [u32] {
        let count = self.width as usize * self.height as usize;
        let off = self.slots[idx].offset;
        // SAFETY: [off, off + count*4) is inside the mmap; &mut self plus the
        // !busy precondition mean nobody else touches it right now.
        unsafe { std::slice::from_raw_parts_mut(self.map.add(off) as *mut u32, count) }
    }
}

impl Drop for Buffers {
    fn drop(&mut self) {
        for s in &self.slots {
            s.buffer.destroy();
        }
        self.pool.destroy();
        // SAFETY: map/map_len came from mmap and are not used after this.
        // Also ignore errors in drop function
        let _ = unsafe { mm::munmap(self.map as *mut std::ffi::c_void, self.map_len) };
    }
}

impl Monitor {
    fn paint(&mut self, qh: &QueueHandle<App>, app: &App) -> anyhow::Result<()> {
        let Layout {
            phys_width: pw,
            phys_height: ph,
            ..
        } = self.layout;

        // Reallocate only when the physical size actually changed.
        let stale = match &self.buffers {
            None => true,
            Some(b) => b.width != pw || b.height != ph,
        };
        if stale {
            let shm = app.shm.as_ref().expect("shm bound");
            // Drop the old pool before creating the new one.
            self.buffers = None;
            self.buffers = Some(Buffers::new(shm, qh, pw, ph)?);
        }

        let Some(idx) = self.buffers.as_ref().expect("just ensured").free_slot() else {
            // Both in flight. Stay dirty -- the release event wakes the loop and
            // we paint then. Never block waiting for a buffer.
            return Ok(());
        };

        let bufs = self.buffers.as_mut().unwrap();

        render::draw(
            bufs.pixels(idx),
            &app.bar,
            &self.tags,
            &self.layout,
            &mut app.glyph_cache,
        );

        let surface = self.surface.as_ref().expect("surface");
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.layout.dest_width, LOGICAL_HEIGHT);
        }
        let bufs = self.buffers.as_ref().expect("checked above");
        surface.attach(Some(&bufs.slots[idx].buffer), 0, 0);
        surface.damage_buffer(0, 0, pw, ph);
        surface.commit();

        // Mark in flight until the compositor sends `release`.
        self.buffers.as_mut().expect("checked above").slots[idx].busy = true;
        self.dirty = false;
        Ok(())
    }
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_output" => {
                dbg!(&name);
                let monitor = Monitor {
                    name: String::new(), // TODO improve
                    dirty: true,
                    tags: [TagState::Empty; 9],
                    wl_output: registry.bind::<WlOutput, _, _>(name, version.min(1), qh, ()),
                    surface: None,
                    viewport: None,
                    layout: Layout::default(),
                    buffers: None,
                };
                state.monitors.push(monitor);
            }
            "wl_compositor" => {
                state.compositor =
                    Some(registry.bind::<WlCompositor, _, _>(name, version.min(6), qh, ()));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ()));
            }
            "zwlr_layer_shell_v1" => {
                dbg!(&name);
                state.layer_shell =
                    Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(5), qh, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.fractional =
                    Some(registry.bind::<WpFractionalScaleManagerV1, _, _>(name, 1, qh, ()));
            }
            "wp_viewporter" => {
                state.viewporter = Some(registry.bind::<WpViewporter, _, _>(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let monitor_idx = state.monitors.iter().position(|m| m.wl_output.id() == proxy.id()).unwrap();
        match event {
            wayland_client::protocol::wl_output::Event::Name { name } => state.monitors[monitor_idx].name = name,
            wayland_client::protocol::wl_output::Event::Done => { state.monitors.remove(monitor_idx); },
            _ => todo!(),
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let monitor: &mut Monitor = todo!();
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, .. } => {
                layer.ack_configure(serial);
                monitor.layout.logical_width = width;
                monitor.dirty = true;
                // TODO rescale monitor layout properly
            }
            zwlr_layer_surface_v1::Event::Closed => state.should_exit = true,
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let monitor: &mut Monitor = todo!();
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && scale != monitor.layout.scale120
        {
            monitor.layout.scale120 = scale;
            // TODO rescale monitor layout properly
            monitor.dirty = true;
        }
    }
}

/// User-data is the slot index, so the handler clears `busy` in place. No Arc or
/// atomics for state only this thread ever touches.
impl Dispatch<WlBuffer, usize> for App {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => {
                if let Some(b) = state.buffers.as_mut()
                    && let Some(s) = b.slots.get_mut(*idx)
                {
                    s.busy = false;
                }
            }
            _ => {}
        }
    }
}

macro_rules! ignore {
    ($($t:ty),* $(,)?) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(),
                     _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(
    WlCompositor,
    WlShm,
    WlShmPool,
    WlSurface,
    ZwlrLayerShellV1,
    WpFractionalScaleManagerV1,
    WpViewporter,
    WpViewport,
);

/// Read the expiration count to clear readability. ECANCELED means the clock was
/// stepped (TFD_TIMER_CANCEL_ON_SET); we re-arm and repaint either way.
fn drain_timer(fd: BorrowedFd<'_>) -> anyhow::Result<()> {
    let mut buf = [0u8; 8];
    match rustix::io::read(fd, &mut buf) {
        Ok(_) | Err(rustix::io::Errno::CANCELED) => (),
        Err(e) => anyhow::bail!(e),
    }
    Ok(())
}

fn load_font() -> fontdue::Font {
    // TODO Load font some proper way
    let font_file = std::fs::read(
        "/nix/store/l9lig0cpdxwyjmhx6yb2pr1lsx29rs78-adwaita-fonts-50.0/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
    ).unwrap();
    fontdue::Font::from_bytes(font_file.as_slice(), fontdue::FontSettings::default()).unwrap()
}

fn initialize_monitor(monitor: &mut Monitor, app: &mut App, qh: &QueueHandle<App>) -> anyhow::Result<()> {
    let compositor = app
        .compositor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no wl_compositor"))?;
    let layer_shell = app
        .layer_shell
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no zwlr_layer_shell_v1"))?;
    if app.shm.is_none() {
        anyhow::bail!("no wl_shm");
    }

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None, // compositor picks the output
        zwlr_layer_shell_v1::Layer::Top,
        "ertragbar".to_string(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    layer.set_size(0, LOGICAL_HEIGHT as u32); // width 0 => span the anchored edges
    layer.set_exclusive_zone(LOGICAL_HEIGHT);

    if let Some(mgr) = app.fractional.take() {
        mgr.get_fractional_scale(&surface, &qh, ());
    }
    if let Some(vp) = app.viewporter.take() {
        monitor.viewport = Some(vp.get_viewport(&surface, &qh, ()));
    }
    surface.commit(); // provoke the first configure
    monitor.surface = Some(surface);

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let font_thread: JoinHandle<fontdue::Font> = std::thread::spawn(load_font);

    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let tags = tags::mmsg_to_tags(tags::poll_tags()?);
    let time = crate::time::poll_time()?;
    let mut app = App {
        monitors: vec![],
        bar: Bar {
            network: Network::WifiDisconnected,
            battery: Battery::None,
            time,
        },
        glyph_cache: GlyphCache::new(font_thread),
        compositor: None,
        layer_shell: None,
        fractional: None,
        viewporter: None,
        shm: None,
        dirty: true,
        should_exit: false,
    };

    queue.roundtrip(&mut app)?; // bind globals

    // queue.roundtrip(&mut app)?; // configure + scale
    // for monitor in app.monitors {
    //     monitor.paint(&qh, &mut app)?;
    // }

    // External event sources.
    let timer = crate::time::minute_timerfd()?;
    let mut child = std::process::Command::new("mmsg")
        .args(["watch", "all-tags"])
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut tags_rd = std::io::BufReader::new(stdout);
    let tags_rd_raw = tags_rd.get_ref().as_raw_fd();
    let battery_path = battery::battery_path();
    let uevent = battery::UeventSocket::new()?;
    let uevent_fd = uevent.as_fd();
    let netlink = network::NetlinkSocket::new()?;
    let netlink_fd = netlink.as_fd();

    // Initial state.
    app.bar.battery = battery::read(&battery_path);
    app.bar.network = network::read();
    app.dirty = true;

    // Reused so the hot path doesn't allocate a String per line.
    let mut line = String::new();

    while !app.should_exit {
        queue.flush()?;

        // None means events are already queued: dispatch them instead of sleeping.
        let Some(guard) = queue.prepare_read() else {
            queue.dispatch_pending(&mut app)?;
            if app.dirty {
                app.paint(&qh)?;
            }
            continue;
        };

        let mut fds = [
            event::PollFd::new(&conn, event::PollFlags::IN),
            event::PollFd::new(&timer, event::PollFlags::IN),
            event::PollFd::from_borrowed_fd(
                unsafe { BorrowedFd::borrow_raw(tags_rd_raw) },
                event::PollFlags::IN,
            ),
            event::PollFd::new(&uevent_fd, event::PollFlags::IN),
            event::PollFd::new(&netlink_fd, event::PollFlags::IN),
        ];

        event::poll(&mut fds, None)?;

        if fds[0].revents().contains(event::PollFlags::IN) {
            guard.read()?;
        } else {
            drop(guard);
        }
        queue.dispatch_pending(&mut app)?;

        if fds[1].revents().contains(event::PollFlags::IN) {
            drain_timer(timer.as_fd())?;
            while let Err(e) = crate::time::arm_next_minute(&timer) {
                eprintln!("Error setting timer: {e}");
            }
            let t = crate::time::poll_time()?;
            if t != (app.bar.time) {
                app.bar.time = t;
                app.dirty = true;
            }

            // Also read battery by the minute (there's no events emited when charging)
            uevent.drain();
            let b = battery::read(&battery_path);
            if b != app.bar.battery {
                app.bar.battery = b;
                app.dirty = true;
            }
        }

        if fds[2].revents().contains(event::PollFlags::IN) {
            // A tag switch can emit several lines at once. Drain everything
            // buffered and keep only the last, so a burst costs one paint.
            let mut latest: Option<Vec<(String, [TagState; 9])>> = None;
            loop {
                line.clear();
                match tags_rd.read_line(&mut line) {
                    Ok(0) => anyhow::bail!("mmsg watch exited"),
                    Ok(_) => {
                        if let Ok(o) = tags::MmsgOutput::deserialize_json(&line) {
                            let latest = Some(tags::mmsg_to_tags(o));
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
                // Nothing more buffered: reading again would block.
                if tags_rd.buffer().is_empty() {
                    break;
                }
            }

            if let Some(tags) = latest {
                for (monitor_name, tags) in tags {
                    let monitor = app.monitors.iter_mut().find(|m| m.name == monitor_name).unwrap();
                    if tags != monitor.tags {
                        monitor.tags = tags;
                        monitor.dirty = true;
                    }
                }
            }
        }

        if fds[3].revents().contains(event::PollFlags::IN) {
            uevent.drain();
            let b = battery::read(&battery_path);
            if b != app.bar.battery {
                app.bar.battery = b;
                app.dirty = true;
            }
        }

        if fds[4].revents().contains(event::PollFlags::IN) {
            netlink.drain();
            let n = network::read();
            if n != app.bar.network {
                app.bar.network = n;
                app.dirty = true;
            }
        }

        for monitor in &mut app.monitors {
            if app.dirty || monitor.dirty {
                monitor.paint(&qh, &app)?;
                monitor.dirty = false;
            }
        }
        app.dirty = false;
    }

    let _ = child.kill();
    Ok(())
}
