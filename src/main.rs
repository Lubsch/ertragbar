use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use rustix::{event, mm};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
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
    battery::Battery,
    network::Network,
    render::LOGICAL_HEIGHT,
    tags::{TagSnapshot, TagState},
    time::Time,
};

mod battery;
mod network;
mod render;
mod socket;
mod tags;
mod time;

/// State shared by every bar.
#[derive(Debug)]
struct Bar {
    network: Network,
    battery: Battery,
    time: Time,
}

const NO_TAGS: [TagState; 9] = [TagState::Empty; 9];

/// The layer surface and the objects hanging off it. Exists from the output's
/// first `done` until the output goes away or the compositor closes it.
struct BarSurface {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Logical width from the latest configure; None until the first one, and
    /// we must not attach a buffer before acking that.
    logical_width: Option<u32>,
}

impl Drop for BarSurface {
    fn drop(&mut self) {
        if let Some(f) = &self.fractional {
            f.destroy();
        }
        if let Some(v) = &self.viewport {
            v.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
    }
}

/// One bar per wl_output. Keyed everywhere by the output's registry `global`
/// name, which every per-output object carries as its user data.
struct Monitor {
    global: u32,
    wl_output: WlOutput,
    /// Connector name ("eDP-1"), matches mmsg's monitor names. Arrives with
    /// wl_output v4's `name` event.
    name: Option<String>,
    /// Got the first `done`, i.e. the output's description is complete.
    described: bool,
    /// The compositor closed our layer surface; don't recreate it.
    closed: bool,
    /// Fractional scale x120 (120 == 1.0).
    scale120: u32,
    layout: Option<Layout>,
    bar_surface: Option<BarSurface>,
    // Declared after `bar_surface` so the surface is destroyed first.
    buffers: Option<Buffers>,
    dirty: bool,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.bar_surface = None;
        self.buffers = None;
        self.wl_output.release();
    }
}

/// The (first) seat's pointer, for clicking tags.
struct Pointer {
    pointer: WlPointer,
    shape: Option<WpCursorShapeDeviceV1>,
    /// Monitor global and surface-local x while the pointer is over a bar.
    focus: Option<(u32, f64)>,
}

impl Drop for Pointer {
    fn drop(&mut self) {
        if let Some(s) = &self.shape {
            s.destroy();
        }
        self.pointer.release();
    }
}

/// linux/input-event-codes.h
const BTN_LEFT: u32 = 0x110;

struct App {
    compositor: Option<WlCompositor>,
    layer_shell: Option<ZwlrLayerShellV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    shm: Option<WlShm>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    pointer: Option<Pointer>,
    /// `mmsg dispatch` processes from clicks, reaped in the main loop.
    clicks: Vec<Child>,
    seat_bound: bool,

    monitors: Vec<Monitor>,
    tags: TagSnapshot,
    glyph_cache: GlyphCache,
    bar: Bar,
}

impl App {
    fn monitor(&mut self, global: u32) -> Option<&mut Monitor> {
        self.monitors.iter_mut().find(|m| m.global == global)
    }

    /// Left click on a bar: switch to the tag under the pointer. Acts on
    /// mango's focused monitor, which follows the pointer onto this bar.
    fn click(&mut self) {
        let Some((global, x)) = self.pointer.as_ref().and_then(|p| p.focus) else {
            return;
        };
        let Some(layout) = self.monitor(global).and_then(|m| m.layout) else {
            return;
        };
        // Pointer coords are logical; tag squares are phys_height wide in
        // buffer pixels.
        let phys_x = x * layout.phys_width as f64 / layout.dest_width as f64;
        let tag = (phys_x / layout.phys_height as f64) as usize;
        if tag >= 9 {
            return;
        }
        match Command::new("mmsg")
            .args(["dispatch", &format!("view,{},0", tag + 1)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(child) => self.clicks.push(child),
            Err(e) => eprintln!("ertragbar: spawning mmsg: {e}"),
        }
    }

    fn reap_clicks(&mut self) {
        self.clicks.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }

    fn set_battery(&mut self, b: Battery) {
        if b != self.bar.battery {
            self.bar.battery = b;
            self.mark_all_dirty();
        }
    }

    fn mark_all_dirty(&mut self) {
        for m in &mut self.monitors {
            m.dirty = true;
        }
    }

    /// Give every fully described output without a bar one. Called after each
    /// dispatch, so it covers both startup and hotplug regardless of the order
    /// globals and output events arrive in.
    fn create_pending_surfaces(&mut self, qh: &QueueHandle<App>) {
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        for m in &mut self.monitors {
            if !m.described || m.closed || m.bar_surface.is_some() {
                continue;
            }
            m.bar_surface = Some(BarSurface::new(
                m.global,
                &m.wl_output,
                compositor,
                layer_shell,
                self.fractional.as_ref(),
                self.viewporter.as_ref(),
                qh,
            ));
        }
    }

    fn recompute_layout(&mut self, global: u32) {
        let Some(m) = self.monitors.iter_mut().find(|m| m.global == global) else {
            return;
        };
        let Some(width) = m.bar_surface.as_ref().and_then(|s| s.logical_width) else {
            return;
        };
        if width == 0 {
            return;
        }
        m.layout = Some(self.glyph_cache.layout(m.scale120, width));
        m.dirty = true;
    }

    fn paint_dirty(&mut self, qh: &QueueHandle<App>) -> anyhow::Result<()> {
        let App {
            monitors,
            shm,
            tags,
            glyph_cache,
            bar,
            ..
        } = self;
        let shm = shm.as_ref().ok_or_else(|| anyhow::anyhow!("no wl_shm"))?;
        for m in monitors.iter_mut().filter(|m| m.dirty) {
            let monitor_tags = m
                .name
                .as_ref()
                .and_then(|n| tags.iter().find(|(name, _)| name == n))
                .map_or(&NO_TAGS, |(_, t)| t);
            m.paint(qh, shm, bar, monitor_tags, glyph_cache)?;
        }
        Ok(())
    }
}

impl BarSurface {
    fn new(
        global: u32,
        output: &WlOutput,
        compositor: &WlCompositor,
        layer_shell: &ZwlrLayerShellV1,
        fractional: Option<&WpFractionalScaleManagerV1>,
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<App>,
    ) -> Self {
        let surface = compositor.create_surface(qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(output),
            zwlr_layer_shell_v1::Layer::Top,
            "ertragbar".to_string(),
            qh,
            global,
        );
        layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
        layer.set_size(0, LOGICAL_HEIGHT as u32); // width 0 => span the anchored edges
        layer.set_exclusive_zone(LOGICAL_HEIGHT);

        let fractional = fractional.map(|mgr| mgr.get_fractional_scale(&surface, qh, global));
        let viewport = viewporter.map(|vp| vp.get_viewport(&surface, qh, ()));
        surface.commit(); // provoke the first configure

        BarSurface {
            surface,
            layer,
            viewport,
            fractional,
            logical_width: None,
        }
    }
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
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<App>,
        global: u32,
        width: i32,
        height: i32,
    ) -> anyhow::Result<Self> {
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
                (global, idx), // user-data: owning monitor + slot, so release can clear `busy`
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
    fn paint(
        &mut self,
        qh: &QueueHandle<App>,
        shm: &WlShm,
        bar: &Bar,
        tags: &[TagState; 9],
        glyph_cache: &mut GlyphCache,
    ) -> anyhow::Result<()> {
        // Not configured yet: stay dirty, the configure triggers the paint.
        let (Some(layout), Some(bs)) = (self.layout, self.bar_surface.as_ref()) else {
            return Ok(());
        };
        let (pw, ph) = (layout.phys_width, layout.phys_height);

        // Reallocate only when the physical size actually changed.
        let stale = match &self.buffers {
            None => true,
            Some(b) => b.width != pw || b.height != ph,
        };
        if stale {
            // Drop the old pool before creating the new one.
            self.buffers = None;
            self.buffers = Some(Buffers::new(shm, qh, self.global, pw, ph)?);
        }
        let bufs = self.buffers.as_mut().expect("just ensured");

        let Some(idx) = bufs.free_slot() else {
            // Both in flight. Stay dirty -- the release event wakes the loop and
            // we paint then. Never block waiting for a buffer.
            return Ok(());
        };

        render::draw(bufs.pixels(idx), bar, tags, &layout, glyph_cache);

        if let Some(vp) = &bs.viewport {
            vp.set_destination(layout.dest_width, LOGICAL_HEIGHT);
        }
        bs.surface.attach(Some(&bufs.slots[idx].buffer), 0, 0);
        bs.surface.damage_buffer(0, 0, pw, ph);
        bs.surface.commit();

        // Mark in flight until the compositor sends `release`.
        bufs.slots[idx].busy = true;
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" => {
                    // v4 brings the `name` event we match mmsg monitors by.
                    if version < 4 {
                        eprintln!("ertragbar: wl_output v{version} < 4, ignoring output");
                        return;
                    }
                    state.monitors.push(Monitor {
                        global: name,
                        wl_output: registry.bind::<WlOutput, _, _>(name, 4, qh, name),
                        name: None,
                        described: false,
                        closed: false,
                        scale120: 120,
                        layout: None,
                        bar_surface: None,
                        buffers: None,
                        dirty: true,
                    });
                }
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<WlCompositor, _, _>(name, version.min(6), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
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
                // One seat is plenty for a bar; extra seats are ignored.
                "wl_seat" if !state.seat_bound => {
                    state.seat_bound = true;
                    registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ());
                }
                "wp_cursor_shape_manager_v1" => {
                    state.cursor_shape =
                        Some(registry.bind::<WpCursorShapeManagerV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            },
            // Output unplugged: dropping the Monitor tears down its bar.
            wl_registry::Event::GlobalRemove { name } => {
                state.monitors.retain(|m| m.global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for App {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(m) = state.monitor(*global) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => {
                m.name = Some(name);
                m.dirty = true;
            }
            // Description complete; create_pending_surfaces picks it up.
            wl_output::Event::Done => m.described = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for App {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(m) = state.monitor(*global) else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, .. } => {
                layer.ack_configure(serial);
                if let Some(bs) = m.bar_surface.as_mut() {
                    bs.logical_width = Some(width);
                }
                state.recompute_layout(*global);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                m.closed = true;
                m.bar_surface = None;
                m.buffers = None;
                m.layout = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, u32> for App {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(m) = state.monitor(*global) else {
            return;
        };
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && scale != 0
            && scale != m.scale120
        {
            m.scale120 = scale;
            state.recompute_layout(*global);
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        let has_pointer = caps.contains(wl_seat::Capability::Pointer);
        if has_pointer && state.pointer.is_none() {
            state.pointer = Some(Pointer {
                pointer: seat.get_pointer(qh, ()),
                shape: None,
                focus: None,
            });
        } else if !has_pointer {
            state.pointer = None; // Drop releases it
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                ..
            } => {
                let global = state
                    .monitors
                    .iter()
                    .find(|m| m.bar_surface.as_ref().is_some_and(|b| b.surface == surface))
                    .map(|m| m.global);
                let Some(p) = state.pointer.as_mut() else {
                    return;
                };
                p.focus = global.map(|g| (g, surface_x));
                // Without this the cursor over the bar is whatever the
                // compositor last drew, or nothing.
                if p.shape.is_none()
                    && let Some(mgr) = &state.cursor_shape
                {
                    p.shape = Some(mgr.get_pointer(&p.pointer, qh, ()));
                }
                if let Some(shape) = &p.shape {
                    shape.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if let Some(p) = state.pointer.as_mut() {
                    p.focus = None;
                }
            }
            wl_pointer::Event::Motion { surface_x, .. } => {
                if let Some((_, x)) = state.pointer.as_mut().and_then(|p| p.focus.as_mut()) {
                    *x = surface_x;
                }
            }
            wl_pointer::Event::Button {
                button: BTN_LEFT,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => state.click(),
            _ => {}
        }
    }
}

/// User-data is (monitor global, slot index), so the handler clears `busy` in
/// place. No Arc or atomics for state only this thread ever touches.
impl Dispatch<WlBuffer, (u32, usize)> for App {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        &(global, idx): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(m) = state.monitor(global)
            && let Some(b) = m.buffers.as_mut()
            && let Some(s) = b.slots.get_mut(idx)
        {
            s.busy = false;
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
    WpCursorShapeManagerV1,
    WpCursorShapeDeviceV1,
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

/// Path baked in at build time by the flake (package and dev shell), so the
/// binary keeps a store reference to the font and it can't be GC'd.
const FONT_PATH: &str = env!("ERTRAGBAR_FONT");

fn load_font() -> fontdue::Font {
    let font_file =
        std::fs::read(FONT_PATH).unwrap_or_else(|e| panic!("reading font {FONT_PATH}: {e}"));
    fontdue::Font::from_bytes(font_file.as_slice(), fontdue::FontSettings::default())
        .unwrap_or_else(|e| panic!("parsing font {FONT_PATH}: {e}"))
}

fn main() -> anyhow::Result<()> {
    let font_thread: JoinHandle<fontdue::Font> = std::thread::spawn(load_font);

    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App {
        compositor: None,
        layer_shell: None,
        fractional: None,
        viewporter: None,
        shm: None,
        cursor_shape: None,
        pointer: None,
        clicks: vec![],
        seat_bound: false,
        monitors: vec![],
        tags: tags::poll_tags()?,
        glyph_cache: GlyphCache::new(font_thread),
        bar: Bar {
            network: Network::WifiDisconnected,
            battery: Battery::None,
            time: crate::time::poll_time()?,
        },
    };

    queue.roundtrip(&mut app)?; // bind globals
    if app.compositor.is_none() {
        anyhow::bail!("no wl_compositor");
    }
    if app.layer_shell.is_none() {
        anyhow::bail!("no zwlr_layer_shell_v1");
    }
    if app.shm.is_none() {
        anyhow::bail!("no wl_shm");
    }
    queue.roundtrip(&mut app)?; // output names + done
    app.create_pending_surfaces(&qh);

    // External event sources.
    let timer = crate::time::minute_timerfd()?;
    let mut tag_watcher = tags::TagWatcher::spawn()?;
    let battery_path = battery::battery_path();
    let uevent = battery::UeventSocket::new()?;
    let netlink = network::NetlinkSocket::new()?;

    // Initial state.
    app.bar.battery = battery::read(&battery_path);
    app.bar.network = network::read();
    app.mark_all_dirty();

    loop {
        queue.flush()?;

        // None means events are already queued: dispatch them instead of sleeping.
        let Some(guard) = queue.prepare_read() else {
            queue.dispatch_pending(&mut app)?;
            app.create_pending_surfaces(&qh);
            app.paint_dirty(&qh)?;
            continue;
        };

        let mut fds = [
            event::PollFd::new(&conn, event::PollFlags::IN),
            event::PollFd::new(&timer, event::PollFlags::IN),
            event::PollFd::from_borrowed_fd(tag_watcher.as_fd(), event::PollFlags::IN),
            event::PollFd::from_borrowed_fd(uevent.as_fd(), event::PollFlags::IN),
            event::PollFd::from_borrowed_fd(netlink.as_fd(), event::PollFlags::IN),
        ];

        event::poll(&mut fds, None)?;
        // HUP too: an exited mmsg shows up as hangup, and read_latest reports it.
        let ready = |i: usize| {
            fds[i]
                .revents()
                .intersects(event::PollFlags::IN | event::PollFlags::HUP)
        };
        let (wl, tick, tags_in, uev, net) = (ready(0), ready(1), ready(2), ready(3), ready(4));

        if wl {
            guard.read()?;
        } else {
            drop(guard);
        }
        queue.dispatch_pending(&mut app)?;
        app.create_pending_surfaces(&qh);

        if tick {
            drain_timer(timer.as_fd())?;
            while let Err(e) = crate::time::arm_next_minute(&timer) {
                eprintln!("Error setting timer: {e}");
            }
            let t = crate::time::poll_time()?;
            if t != app.bar.time {
                app.bar.time = t;
                app.mark_all_dirty();
            }

            // Also read battery by the minute (there's no events emited when charging)
            uevent.drain();
            app.set_battery(battery::read(&battery_path));
        }

        if tags_in && let Some(new) = tag_watcher.read_latest()? {
            // Repaint only monitors whose tags actually changed.
            for m in &mut app.monitors {
                let Some(name) = &m.name else { continue };
                let lookup =
                    |snap: &TagSnapshot| snap.iter().find(|(n, _)| n == name).map(|(_, t)| *t);
                if lookup(&app.tags) != lookup(&new) {
                    m.dirty = true;
                }
            }
            app.tags = new;
        }

        if uev {
            uevent.drain();
            app.set_battery(battery::read(&battery_path));
        }

        if net {
            netlink.drain();
            let n = network::read();
            if n != app.bar.network {
                app.bar.network = n;
                app.mark_all_dirty();
            }
        }

        app.paint_dirty(&qh)?;
        app.reap_clicks();
    }
}
