//! A Wayland session: finds the globals it needs, owns layer-shell surfaces
//! and their shared-memory buffers, and turns pointer events into plain
//! `Event`s. Everything runs on the caller's thread; the only other thread is
//! the socket reader in `super::Conn`.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

use super::proto::*;
use super::wire::{Args, Msg, Reader};
use super::{socket_path, Conn, Incoming};
use crate::gfx::canvas::Canvas;
use crate::sys::{memfd, Map};

pub use super::proto::cursor_shape_device::{SHAPE_DEFAULT, SHAPE_POINTER};
pub use super::proto::layer_shell::{LAYER_OVERLAY, LAYER_TOP};
pub use super::proto::layer_surface::{ANCHOR_BOTTOM, ANCHOR_LEFT, ANCHOR_RIGHT, ANCHOR_TOP};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// How a layer surface is created.
#[derive(Clone, Debug)]
pub struct LayerOpts {
    pub layer: u32,
    pub anchor: u32,
    /// Requested size in logical pixels; 0 lets the compositor choose that axis.
    pub width: u32,
    pub height: u32,
    /// top, right, bottom, left
    pub margin: [i32; 4],
    /// 0 stays clear of bars (waybar's exclusive zone), -1 ignores them.
    pub zone: i32,
    pub namespace: String,
    /// An invisible click catcher: nothing is drawn, only input is received.
    pub blank: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The surface has a size (or a new one): draw it now.
    Configure { sid: usize, w: u32, h: u32 },
    /// The compositor removed the surface.
    Closed(usize),
    /// The scale changed, so the buffer size did too: draw again.
    ScaleChanged(usize),
    /// A buffer came free while a frame was waiting for one.
    Redraw(usize),
    PointerEnter { sid: usize, x: f32, y: f32 },
    PointerLeave { sid: usize },
    PointerMotion { sid: usize, x: f32, y: f32 },
    PointerButton { sid: usize, x: f32, y: f32, button: u32, pressed: bool },
    /// Vertical scroll in logical pixels (positive is down).
    Scroll { sid: usize, dy: f32 },
    /// The connection is gone; nothing works after this.
    Disconnected(String),
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Display,
    Registry,
    Sync,
    Compositor,
    Shm,
    ShmPool,
    Region,
    Seat,
    Pointer,
    CursorMgr,
    CursorDev,
    LayerShell,
    Viewporter,
    FracMgr,
    Output,
    Surface(usize),
    LayerSurface(usize),
    Viewport,
    Frac(usize),
    Buffer(usize),
}

struct Global {
    name: u32,
    interface: String,
    version: u32,
    bound: bool,
}

struct Buf {
    id: u32,
    w: u32,
    h: u32,
    map: Map,
    busy: bool,
}

struct Surf {
    opts: LayerOpts,
    wl: u32,
    layer: u32,
    viewport: u32,
    frac: u32,
    /// Logical size from the last configure.
    lw: u32,
    lh: u32,
    configured: bool,
    /// Preferred scale from wp_fractional_scale (120ths), if any.
    frac_pref: u32,
    /// Preferred integer scale from wl_surface (v6), if any.
    int_pref: u32,
    entered: Vec<u32>,
    bufs: Vec<Buf>,
    /// A frame was requested while every buffer was in use.
    starved: bool,
    last_scale120: u32,
}

pub struct Wl {
    conn: Conn,
    rx: Option<Receiver<Incoming>>,
    kinds: HashMap<u32, Kind>,
    globals: Vec<Global>,
    registry: u32,
    compositor: (u32, u32),
    shm: u32,
    seat: (u32, u32),
    layer_shell: u32,
    viewporter: u32,
    frac_mgr: u32,
    cursor_mgr: u32,
    pointer: u32,
    cursor_dev: u32,
    outputs: HashMap<u32, i32>,
    surfaces: HashMap<usize, Surf>,
    next_sid: usize,
    focus: Option<usize>,
    pos: (f32, f32),
    enter_serial: u32,
    last_shape: Option<(u32, u32)>,
    synced: HashSet<u32>,
    dead: Option<String>,
    ready: bool,
}

type R<T> = Result<T, String>;

impl Wl {
    /// Connect to `$WAYLAND_DISPLAY` and bind the globals we use.
    pub fn connect() -> R<Wl> {
        Wl::connect_at(&socket_path()?)
    }

    pub fn connect_at(path: &std::path::Path) -> R<Wl> {
        let (tx, rx) = channel();
        let conn = Conn::connect(path, tx).map_err(|e| format!("cannot connect to the Wayland compositor at {}: {e}", path.display()))?;
        let mut wl = Wl {
            conn,
            rx: Some(rx),
            kinds: HashMap::new(),
            globals: Vec::new(),
            registry: 0,
            compositor: (0, 0),
            shm: 0,
            seat: (0, 0),
            layer_shell: 0,
            viewporter: 0,
            frac_mgr: 0,
            cursor_mgr: 0,
            pointer: 0,
            cursor_dev: 0,
            outputs: HashMap::new(),
            surfaces: HashMap::new(),
            next_sid: 1,
            focus: None,
            pos: (0.0, 0.0),
            enter_serial: 0,
            last_shape: None,
            synced: HashSet::new(),
            dead: None,
            ready: false,
        };
        wl.kinds.insert(1, Kind::Display);
        wl.registry = wl.new_obj(Kind::Registry);
        let reg = wl.registry;
        wl.send(1, display::GET_REGISTRY, Args::new().u(reg))?;
        wl.roundtrip()?;
        wl.bind_new()?;
        if wl.compositor.0 == 0 || wl.shm == 0 {
            return Err("the compositor lacks wl_compositor or wl_shm".into());
        }
        if wl.layer_shell == 0 {
            return Err("this compositor does not offer the wlr-layer-shell protocol, which notify-rs needs to draw its panel \
                        (Sway, Hyprland, niri, river, labwc, Wayfire and KDE Plasma have it; GNOME's Mutter does not)"
                .into());
        }
        wl.roundtrip()?; // seat capabilities, output scales
        wl.ready = true;
        Ok(wl)
    }

    /// The channel the reader thread feeds. Take it once, forward it into your
    /// own event loop and hand every item back to `handle`.
    pub fn take_receiver(&mut self) -> Receiver<Incoming> {
        self.rx.take().expect("receiver already taken")
    }

    pub fn is_dead(&self) -> Option<&str> {
        self.dead.as_deref()
    }

    pub fn has_cursor_shapes(&self) -> bool {
        self.cursor_dev != 0
    }

    pub fn has_fractional_scale(&self) -> bool {
        self.frac_mgr != 0 && self.viewporter != 0
    }

    // ------------------------------------------------------------ plumbing

    fn new_obj(&mut self, k: Kind) -> u32 {
        let id = self.conn.alloc();
        self.kinds.insert(id, k);
        id
    }

    fn send(&mut self, obj: u32, op: u16, args: Args) -> R<()> {
        if let Some(d) = &self.dead {
            return Err(d.clone());
        }
        self.conn.send(obj, op, args).map_err(|e| {
            let m = format!("writing to the compositor failed: {e}");
            self.dead = Some(m.clone());
            m
        })
    }

    fn send_fd(&mut self, obj: u32, op: u16, args: Args, fd: std::os::fd::RawFd) -> R<()> {
        if let Some(d) = &self.dead {
            return Err(d.clone());
        }
        self.conn.send_fd(obj, op, args, fd).map_err(|e| {
            let m = format!("writing to the compositor failed: {e}");
            self.dead = Some(m.clone());
            m
        })
    }

    /// Wait until the compositor has processed everything sent so far.
    fn roundtrip(&mut self) -> R<()> {
        let cb = self.new_obj(Kind::Sync);
        self.send(1, display::SYNC, Args::new().u(cb))?;
        loop {
            if self.synced.remove(&cb) {
                return Ok(());
            }
            let rx = self.rx.as_ref().ok_or("receiver already taken")?;
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(inc) => {
                    let mut ev = Vec::new();
                    self.handle_into(inc, &mut ev);
                    if let Some(Event::Disconnected(m)) = ev.into_iter().find(|e| matches!(e, Event::Disconnected(_))) {
                        return Err(m);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Err("the compositor did not answer".into()),
                Err(RecvTimeoutError::Disconnected) => return Err("the compositor connection ended".into()),
            }
        }
    }

    fn bind_new(&mut self) -> R<()> {
        let mut todo = Vec::new();
        for (i, g) in self.globals.iter().enumerate() {
            if g.bound {
                continue;
            }
            let wanted = match g.interface.as_str() {
                "wl_compositor" => self.compositor.0 == 0,
                "wl_shm" => self.shm == 0,
                "wl_seat" => self.seat.0 == 0,
                "zwlr_layer_shell_v1" => self.layer_shell == 0,
                "wp_viewporter" => self.viewporter == 0,
                "wp_fractional_scale_manager_v1" => self.frac_mgr == 0,
                "wp_cursor_shape_manager_v1" => self.cursor_mgr == 0,
                "wl_output" => true,
                _ => false,
            };
            if wanted {
                todo.push(i);
            }
        }
        for i in todo {
            let (name, iface, ver) = {
                let g = &mut self.globals[i];
                g.bound = true;
                (g.name, g.interface.clone(), g.version)
            };
            let (kind, max) = match iface.as_str() {
                "wl_compositor" => (Kind::Compositor, 6),
                "wl_shm" => (Kind::Shm, 1),
                "wl_seat" => (Kind::Seat, 5),
                "zwlr_layer_shell_v1" => (Kind::LayerShell, 4),
                "wp_viewporter" => (Kind::Viewporter, 1),
                "wp_fractional_scale_manager_v1" => (Kind::FracMgr, 1),
                "wp_cursor_shape_manager_v1" => (Kind::CursorMgr, 1),
                _ => (Kind::Output, 3),
            };
            let version = ver.min(max);
            let id = self.new_obj(kind);
            let reg = self.registry;
            self.send(reg, registry::BIND, Args::new().u(name).s(&iface).u(version).u(id))?;
            match kind {
                Kind::Compositor => self.compositor = (id, version),
                Kind::Shm => self.shm = id,
                Kind::Seat => self.seat = (id, version),
                Kind::LayerShell => self.layer_shell = id,
                Kind::Viewporter => self.viewporter = id,
                Kind::FracMgr => self.frac_mgr = id,
                Kind::CursorMgr => self.cursor_mgr = id,
                Kind::Output => {
                    self.outputs.insert(id, 1);
                }
                _ => {}
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ events

    /// Process one item from the receiver; the events it caused.
    pub fn handle(&mut self, inc: Incoming) -> Vec<Event> {
        let mut out = Vec::new();
        self.handle_into(inc, &mut out);
        out
    }

    fn handle_into(&mut self, inc: Incoming, out: &mut Vec<Event>) {
        match inc {
            Incoming::Closed(m) => {
                self.dead = Some(m.clone());
                out.push(Event::Disconnected(m));
            }
            Incoming::Msg(m) => self.dispatch(m, out),
        }
    }

    fn fail(&mut self, msg: String, out: &mut Vec<Event>) {
        self.dead = Some(msg.clone());
        out.push(Event::Disconnected(msg));
    }

    fn sid_of(&self, surface_obj: u32) -> Option<usize> {
        match self.kinds.get(&surface_obj) {
            Some(Kind::Surface(s)) if self.surfaces.contains_key(s) => Some(*s),
            _ => None,
        }
    }

    fn dispatch(&mut self, m: Msg, out: &mut Vec<Event>) {
        let Some(kind) = self.kinds.get(&m.obj).copied() else { return };
        let mut r = Reader::new(&m.data);
        match (kind, m.op) {
            (Kind::Display, display::ERROR) => {
                let (obj, code, text) = (r.u().unwrap_or(0), r.u().unwrap_or(0), r.string().unwrap_or_default());
                self.fail(format!("Wayland protocol error {code} on object {obj}: {text}"), out);
            }
            (Kind::Display, display::DELETE_ID) => {
                if let Some(id) = r.u() {
                    self.kinds.remove(&id);
                    self.conn.recycle(id);
                }
            }
            (Kind::Registry, registry::GLOBAL) => {
                if let (Some(name), Some(interface), Some(version)) = (r.u(), r.string(), r.u()) {
                    self.globals.push(Global { name, interface, version, bound: false });
                    if self.ready {
                        if let Err(e) = self.bind_new() {
                            self.fail(e, out);
                        }
                    }
                }
            }
            (Kind::Sync, callback::DONE) => {
                self.synced.insert(m.obj);
            }
            (Kind::Seat, seat::CAPABILITIES) => {
                if let Some(caps) = r.u() {
                    if let Err(e) = self.seat_caps(caps) {
                        self.fail(e, out);
                    }
                }
            }
            (Kind::Output, output::SCALE) => {
                if let Some(s) = r.i() {
                    self.outputs.insert(m.obj, s.max(1));
                    self.rescale(out);
                }
            }
            (Kind::Surface(sid), surface::ENTER) => {
                if let (Some(o), Some(s)) = (r.u(), self.surfaces.get_mut(&sid)) {
                    if !s.entered.contains(&o) {
                        s.entered.push(o);
                    }
                    self.rescale(out);
                }
            }
            (Kind::Surface(sid), surface::LEAVE) => {
                if let (Some(o), Some(s)) = (r.u(), self.surfaces.get_mut(&sid)) {
                    s.entered.retain(|e| *e != o);
                    self.rescale(out);
                }
            }
            (Kind::Surface(sid), surface::PREFERRED_BUFFER_SCALE) => {
                if let (Some(v), Some(s)) = (r.i(), self.surfaces.get_mut(&sid)) {
                    s.int_pref = v.max(1) as u32;
                    self.rescale(out);
                }
            }
            (Kind::Frac(sid), fractional_scale::PREFERRED_SCALE) => {
                if let (Some(v), Some(s)) = (r.u(), self.surfaces.get_mut(&sid)) {
                    s.frac_pref = v.clamp(30, 120 * 8);
                    self.rescale(out);
                }
            }
            (Kind::LayerSurface(sid), layer_surface::CONFIGURE) => {
                if let (Some(serial), Some(w), Some(h)) = (r.u(), r.u(), r.u()) {
                    let layer = match self.surfaces.get_mut(&sid) {
                        Some(s) => {
                            s.lw = if w > 0 { w } else { s.opts.width.max(1) };
                            s.lh = if h > 0 { h } else { s.opts.height.max(1) };
                            s.configured = true;
                            (s.layer, s.opts.blank, s.lw, s.lh)
                        }
                        None => return,
                    };
                    if let Err(e) = self.send(layer.0, layer_surface::ACK_CONFIGURE, Args::new().u(serial)) {
                        return self.fail(e, out);
                    }
                    if layer.1 {
                        if let Err(e) = self.commit_buffer(sid, None).map(|_| ()) {
                            return self.fail(e, out);
                        }
                    } else {
                        out.push(Event::Configure { sid, w: layer.2, h: layer.3 });
                    }
                }
            }
            (Kind::LayerSurface(sid), layer_surface::CLOSED) => {
                if self.surfaces.contains_key(&sid) {
                    let _ = self.destroy(sid);
                    out.push(Event::Closed(sid));
                }
            }
            (Kind::Buffer(sid), buffer::RELEASE) => {
                let mut redraw = false;
                if let Some(s) = self.surfaces.get_mut(&sid) {
                    if let Some(b) = s.bufs.iter_mut().find(|b| b.id == m.obj) {
                        b.busy = false;
                    }
                    if s.starved {
                        s.starved = false;
                        redraw = true;
                    }
                }
                self.drop_stale_buffers(sid);
                if redraw {
                    out.push(Event::Redraw(sid));
                }
            }
            (Kind::Pointer, pointer::ENTER) => {
                if let (Some(serial), Some(surf), Some(x), Some(y)) = (r.u(), r.u(), r.fixed(), r.fixed()) {
                    if let Some(sid) = self.sid_of(surf) {
                        self.focus = Some(sid);
                        self.pos = (x as f32, y as f32);
                        self.enter_serial = serial;
                        self.last_shape = None;
                        out.push(Event::PointerEnter { sid, x: x as f32, y: y as f32 });
                    }
                }
            }
            (Kind::Pointer, pointer::LEAVE) => {
                if let Some(sid) = self.focus.take() {
                    out.push(Event::PointerLeave { sid });
                }
            }
            (Kind::Pointer, pointer::MOTION) => {
                if let (Some(_t), Some(x), Some(y), Some(sid)) = (r.u(), r.fixed(), r.fixed(), self.focus) {
                    self.pos = (x as f32, y as f32);
                    out.push(Event::PointerMotion { sid, x: x as f32, y: y as f32 });
                }
            }
            (Kind::Pointer, pointer::BUTTON) => {
                if let (Some(_s), Some(_t), Some(button), Some(state), Some(sid)) = (r.u(), r.u(), r.u(), r.u(), self.focus) {
                    out.push(Event::PointerButton { sid, x: self.pos.0, y: self.pos.1, button, pressed: state == pointer::BUTTON_PRESSED });
                }
            }
            (Kind::Pointer, pointer::AXIS) => {
                if let (Some(_t), Some(axis), Some(v), Some(sid)) = (r.u(), r.u(), r.fixed(), self.focus) {
                    if axis == pointer::AXIS_VERTICAL && v != 0.0 {
                        out.push(Event::Scroll { sid, dy: v as f32 });
                    }
                }
            }
            _ => {}
        }
    }

    fn seat_caps(&mut self, caps: u32) -> R<()> {
        let has = caps & seat::CAP_POINTER != 0;
        if has && self.pointer == 0 {
            let p = self.new_obj(Kind::Pointer);
            let seat = self.seat.0;
            self.send(seat, seat::GET_POINTER, Args::new().u(p))?;
            self.pointer = p;
            if self.cursor_mgr != 0 {
                let d = self.new_obj(Kind::CursorDev);
                let mgr = self.cursor_mgr;
                self.send(mgr, cursor_shape_manager::GET_POINTER, Args::new().u(d).u(p))?;
                self.cursor_dev = d;
            }
        } else if !has && self.pointer != 0 {
            if self.cursor_dev != 0 {
                let d = self.cursor_dev;
                self.send(d, cursor_shape_device::DESTROY, Args::new())?;
                self.cursor_dev = 0;
            }
            if self.seat.1 >= 3 {
                let p = self.pointer;
                self.send(p, pointer::RELEASE, Args::new())?;
            }
            self.pointer = 0;
            self.focus = None;
        }
        Ok(())
    }

    // ------------------------------------------------------------ scale

    fn scale120_of(&self, s: &Surf) -> u32 {
        if s.frac_pref > 0 && s.viewport != 0 {
            return s.frac_pref;
        }
        let integer = if s.int_pref > 0 {
            s.int_pref
        } else {
            s.entered.iter().filter_map(|o| self.outputs.get(o)).copied().max().unwrap_or(1).max(1) as u32
        };
        // with a viewport the fractional number is honoured; without one only whole scales work
        if s.frac_pref > 0 {
            return s.frac_pref.div_ceil(120) * 120;
        }
        integer * 120
    }

    fn rescale(&mut self, out: &mut Vec<Event>) {
        let ids: Vec<usize> = self.surfaces.keys().copied().collect();
        for sid in ids {
            let now = self.scale120_of(&self.surfaces[&sid]);
            let s = self.surfaces.get_mut(&sid).unwrap();
            if now != s.last_scale120 {
                s.last_scale120 = now;
                if s.configured && !s.opts.blank {
                    out.push(Event::ScaleChanged(sid));
                } else if s.configured {
                    let _ = self.commit_buffer(sid, None);
                }
            }
        }
    }

    /// Effective scale of a surface (1.0, 1.25, 2.0, ...).
    pub fn scale(&self, sid: usize) -> f32 {
        self.surfaces.get(&sid).map(|s| self.scale120_of(s) as f32 / 120.0).unwrap_or(1.0)
    }

    pub fn logical_size(&self, sid: usize) -> Option<(u32, u32)> {
        self.surfaces.get(&sid).filter(|s| s.configured).map(|s| (s.lw, s.lh))
    }

    /// Size of the buffer the surface needs right now, in device pixels.
    pub fn buffer_size(&self, sid: usize) -> Option<(u32, u32)> {
        let s = self.surfaces.get(&sid).filter(|s| s.configured)?;
        if s.opts.blank && s.viewport != 0 {
            return Some((1, 1));
        }
        let k = self.scale120_of(s);
        if s.viewport != 0 {
            Some(((s.lw * k + 60) / 120, (s.lh * k + 60) / 120))
        } else {
            let int = k / 120;
            Some((s.lw * int, s.lh * int))
        }
    }

    // ------------------------------------------------------------ surfaces

    pub fn create_layer(&mut self, opts: LayerOpts) -> R<usize> {
        let sid = self.next_sid;
        self.next_sid += 1;
        let (comp, layer_shell) = (self.compositor.0, self.layer_shell);
        let wl = self.new_obj(Kind::Surface(sid));
        self.send(comp, compositor::CREATE_SURFACE, Args::new().u(wl))?;
        let layer = self.new_obj(Kind::LayerSurface(sid));
        self.send(layer_shell, layer_shell::GET_LAYER_SURFACE, Args::new().u(layer).u(wl).u(0).u(opts.layer).s(&opts.namespace))?;
        self.send(layer, layer_surface::SET_SIZE, Args::new().u(opts.width).u(opts.height))?;
        self.send(layer, layer_surface::SET_ANCHOR, Args::new().u(opts.anchor))?;
        self.send(layer, layer_surface::SET_EXCLUSIVE_ZONE, Args::new().i(opts.zone))?;
        let m = opts.margin;
        self.send(layer, layer_surface::SET_MARGIN, Args::new().i(m[0]).i(m[1]).i(m[2]).i(m[3]))?;
        self.send(layer, layer_surface::SET_KEYBOARD_INTERACTIVITY, Args::new().u(layer_surface::KEYBOARD_NONE))?;
        let (mut viewport, mut frac) = (0, 0);
        if self.viewporter != 0 {
            viewport = self.new_obj(Kind::Viewport);
            let vp = self.viewporter;
            self.send(vp, viewporter::GET_VIEWPORT, Args::new().u(viewport).u(wl))?;
            if self.frac_mgr != 0 {
                frac = self.new_obj(Kind::Frac(sid));
                let fm = self.frac_mgr;
                self.send(fm, fractional_scale_manager::GET_FRACTIONAL_SCALE, Args::new().u(frac).u(wl))?;
            }
        }
        self.surfaces.insert(
            sid,
            Surf {
                opts,
                wl,
                layer,
                viewport,
                frac,
                lw: 0,
                lh: 0,
                configured: false,
                frac_pref: 0,
                int_pref: 0,
                entered: Vec::new(),
                bufs: Vec::new(),
                starved: false,
                last_scale120: 120,
            },
        );
        self.send(wl, surface::COMMIT, Args::new())?; // asks for the first configure
        Ok(sid)
    }

    /// Ask for a different size (e.g. the panel grew); a Configure event follows.
    pub fn resize(&mut self, sid: usize, w: u32, h: u32) -> R<()> {
        let Some(s) = self.surfaces.get_mut(&sid) else { return Ok(()) };
        if s.opts.width == w && s.opts.height == h {
            return Ok(());
        }
        s.opts.width = w;
        s.opts.height = h;
        let (layer, wl) = (s.layer, s.wl);
        self.send(layer, layer_surface::SET_SIZE, Args::new().u(w).u(h))?;
        self.send(wl, surface::COMMIT, Args::new())
    }

    /// Only this rectangle (logical pixels) receives pointer input; the rest of
    /// the surface lets clicks through to whatever is behind it. Call it after
    /// the first `present`.
    pub fn set_input_rect(&mut self, sid: usize, x: i32, y: i32, w: i32, h: i32) -> R<()> {
        let Some((wl, shown)) = self.surfaces.get(&sid).map(|s| (s.wl, s.configured && !s.bufs.is_empty())) else { return Ok(()) };
        let (comp, region) = (self.compositor.0, self.new_obj(Kind::Region));
        self.send(comp, compositor::CREATE_REGION, Args::new().u(region))?;
        self.send(region, region::ADD, Args::new().i(x).i(y).i(w).i(h))?;
        self.send(wl, surface::SET_INPUT_REGION, Args::new().u(region))?;
        self.send(region, region::DESTROY, Args::new())?;
        if shown {
            self.send(wl, surface::COMMIT, Args::new())?; // the region applies with the next commit
        }
        Ok(())
    }

    pub fn destroy(&mut self, sid: usize) -> R<()> {
        let Some(s) = self.surfaces.remove(&sid) else { return Ok(()) };
        if self.focus == Some(sid) {
            self.focus = None;
        }
        let mut res = self.send(s.layer, layer_surface::DESTROY, Args::new());
        if s.frac != 0 {
            res = res.and(self.send(s.frac, fractional_scale::DESTROY, Args::new()));
        }
        if s.viewport != 0 {
            res = res.and(self.send(s.viewport, viewport::DESTROY, Args::new()));
        }
        res = res.and(self.send(s.wl, surface::DESTROY, Args::new()));
        for b in &s.bufs {
            res = res.and(self.send(b.id, buffer::DESTROY, Args::new()));
        }
        res
    }

    pub fn surface_ids(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.surfaces.keys().copied().collect();
        v.sort_unstable();
        v
    }

    // ------------------------------------------------------------ drawing

    fn drop_stale_buffers(&mut self, sid: usize) {
        let Some((bw, bh)) = self.buffer_size(sid) else { return };
        let mut gone = Vec::new();
        if let Some(s) = self.surfaces.get_mut(&sid) {
            s.bufs.retain(|b| {
                let stale = (b.w, b.h) != (bw, bh) && !b.busy;
                if stale {
                    gone.push(b.id);
                }
                !stale
            });
        }
        for id in gone {
            let _ = self.send(id, buffer::DESTROY, Args::new());
        }
    }

    fn new_buffer(&mut self, sid: usize, w: u32, h: u32) -> R<usize> {
        let stride = w as usize * 4;
        let size = stride * h as usize;
        let file = memfd("notify-rs", size as u64).map_err(|e| format!("cannot create shared memory: {e}"))?;
        let map = Map::shared_rw(&file, size).map_err(|e| format!("cannot map shared memory: {e}"))?;
        let pool = self.new_obj(Kind::ShmPool);
        let shm = self.shm;
        use std::os::fd::AsRawFd;
        self.send_fd(shm, shm::CREATE_POOL, Args::new().u(pool).i(size as i32), file.as_raw_fd())?;
        let id = self.new_obj(Kind::Buffer(sid));
        self.send(pool, shm_pool::CREATE_BUFFER, Args::new().u(id).i(0).i(w as i32).i(h as i32).i(stride as i32).u(shm::FORMAT_ARGB8888))?;
        // the buffer keeps the memory alive; the pool object is no longer needed
        self.send(pool, shm_pool::DESTROY, Args::new())?;
        let s = self.surfaces.get_mut(&sid).ok_or("surface is gone")?;
        s.bufs.push(Buf { id, w, h, map, busy: false });
        Ok(s.bufs.len() - 1)
    }

    /// Show `canvas` (device pixels, see `buffer_size`) on the surface.
    /// Ok(false): every buffer is still held by the compositor; a
    /// `Redraw` event follows when one is free.
    pub fn present(&mut self, sid: usize, canvas: &Canvas) -> R<bool> {
        self.commit_buffer(sid, Some(canvas))
    }

    fn commit_buffer(&mut self, sid: usize, canvas: Option<&Canvas>) -> R<bool> {
        let (bw, bh) = self.buffer_size(sid).ok_or("surface is not configured yet")?;
        if let Some(c) = canvas {
            if (c.w as u32, c.h as u32) != (bw, bh) {
                return Err(format!("canvas is {}x{} but the surface needs {bw}x{bh}", c.w, c.h));
            }
        }
        self.drop_stale_buffers(sid);
        let idx = {
            let s = &self.surfaces[&sid];
            s.bufs.iter().position(|b| !b.busy && (b.w, b.h) == (bw, bh))
        };
        let idx = match idx {
            Some(i) => i,
            None => {
                if self.surfaces[&sid].bufs.len() >= 3 {
                    self.surfaces.get_mut(&sid).unwrap().starved = true;
                    return Ok(false);
                }
                self.new_buffer(sid, bw, bh)?
            }
        };
        let k = self.scale120_of(&self.surfaces[&sid]);
        let s = self.surfaces.get_mut(&sid).unwrap();
        {
            let b = &mut s.bufs[idx];
            let bytes = b.map.as_mut_slice();
            match canvas {
                Some(c) => c.write_to(&mut bytes[..bw as usize * bh as usize * 4]),
                None => bytes.fill(0),
            }
            b.busy = true;
        }
        let (buf_id, wl, vp, lw, lh) = (s.bufs[idx].id, s.wl, s.viewport, s.lw, s.lh);
        s.starved = false;
        if vp != 0 {
            self.send(vp, viewport::SET_DESTINATION, Args::new().i(lw as i32).i(lh as i32))?;
        } else {
            self.send(wl, surface::SET_BUFFER_SCALE, Args::new().i((k / 120) as i32))?;
        }
        self.send(wl, surface::ATTACH, Args::new().u(buf_id).i(0).i(0))?;
        self.send(wl, surface::DAMAGE_BUFFER, Args::new().i(0).i(0).i(bw as i32).i(bh as i32))?;
        self.send(wl, surface::COMMIT, Args::new())?;
        Ok(true)
    }

    /// Set the pointer image (`SHAPE_DEFAULT`, `SHAPE_POINTER`); a no-op
    /// without wp_cursor_shape or while the pointer is outside our surfaces.
    pub fn set_cursor(&mut self, shape: u32) {
        if self.cursor_dev == 0 || self.focus.is_none() {
            return;
        }
        if self.last_shape == Some((self.enter_serial, shape)) {
            return;
        }
        self.last_shape = Some((self.enter_serial, shape));
        let (dev, serial) = (self.cursor_dev, self.enter_serial);
        let _ = self.send(dev, cursor_shape_device::SET_SHAPE, Args::new().u(serial).u(shape));
    }

    pub fn shutdown(&mut self) {
        for sid in self.surface_ids() {
            let _ = self.destroy(sid);
        }
        self.conn.shutdown();
    }
}
