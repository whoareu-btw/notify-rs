//! The daemon with a window: one loop that owns the notification service, the
//! Wayland connection and the media state. Everything that can block (D-Bus
//! calls to players, image decoding) runs on a helper thread and reports back
//! through the same event channel.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::paint::Painter;
use super::panel::{self, Art, Frame, HitId, MediaView, PanelState, MARGIN};
use crate::art;
use crate::dbus::{Conn, Message};
use crate::gfx::canvas::Canvas;
use crate::gfx::text::Fonts;
use crate::model::{PositionClock, Snapshot, Status};
use crate::mpris;
use crate::notify::Notification;
use crate::service::{self, PanelCmd, Service};
use crate::wl::client::*;
use crate::wl::Incoming;

enum Ev {
    Bus(Message),
    BusGone,
    Wl(Incoming),
    Media(Option<Snapshot>),
    Image(String, usize, Option<Arc<Vec<u8>>>),
}

enum MediaCmd {
    PlayPause(String),
    Next(String),
    Prev(String),
    Volume(String, f64),
    Seek { bus: String, track: Option<String>, target_us: i64, current_us: i64 },
}

// ------------------------------------------------------------------ images

enum Slot {
    Loading,
    Failed,
    Ready(Arc<Vec<u8>>),
}

#[derive(Default)]
struct Images {
    map: HashMap<(String, usize), Slot>,
}

impl Images {
    fn get(&self, key: &str, dev: usize) -> Option<Arc<Vec<u8>>> {
        match self.map.get(&(key.to_string(), dev)) {
            Some(Slot::Ready(p)) => Some(p.clone()),
            _ => None,
        }
    }

    /// Make sure `source` will be available at `dev` pixels; loads off-thread.
    fn want(&mut self, source: &str, dev: usize, tx: &Sender<Ev>) {
        let key = (source.to_string(), dev);
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() > 160 {
            self.map.clear();
        }
        self.map.insert(key, Slot::Loading);
        let (tx, source) = (tx.clone(), source.to_string());
        std::thread::spawn(move || {
            let px = art::load_square(&source, dev).map(Arc::new);
            let _ = tx.send(Ev::Image(source, dev, px));
        });
    }

    /// Raw pixels from a notification's image hint (already in memory).
    fn want_raw(&mut self, key: &str, dev: usize, img: &crate::notify::Image) {
        let k = (key.to_string(), dev);
        if self.map.contains_key(&k) {
            return;
        }
        if self.map.len() > 160 {
            self.map.clear();
        }
        let px = art::square_from_rgba(&img.rgba, img.w as usize, img.h as usize, dev);
        self.map.insert(k, Slot::Ready(Arc::new(px)));
    }
}

/// Cache key and loadable source of a notification's picture.
fn note_source(n: &Notification) -> Option<String> {
    if let Some(img) = &n.image {
        return Some(format!("raw:{}:{}:{}x{}", n.id, n.created_unix, img.w, img.h));
    }
    [n.image_path.as_deref(), Some(n.app_icon.as_str())].into_iter().flatten().find(|s| art::loadable(s)).map(str::to_string)
}

// ------------------------------------------------------------------ windows

struct PanelWin {
    sid: usize,
    scrim: usize,
    frame: Frame,
    hover: Option<HitId>,
    pressed: Option<HitId>,
    drag: Option<(HitId, f64)>,
    scroll: f32,
    dirty: bool,
    last_second: i64,
}

struct ToastWin {
    sid: usize,
    frame: Frame,
    hover: Option<HitId>,
    pressed: Option<HitId>,
    dirty: bool,
}

struct MediaState {
    snap: Snapshot,
    clock: PositionClock,
}

struct App {
    svc: Service,
    wl: Wl,
    fonts: Fonts,
    tx: Sender<Ev>,
    cmds: Sender<MediaCmd>,
    images: Images,
    media: Option<MediaState>,
    panel: Option<PanelWin>,
    toast: Option<ToastWin>,
    premute: f64,
    fatal: Option<String>,
}

pub fn run(conn: Conn, inbox: Receiver<Message>) -> Result<(), String> {
    let fonts = Fonts::system()?;
    let mut wl = Wl::connect()?;
    service::claim_names(&conn)?;

    let (tx, rx) = channel::<Ev>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for m in inbox {
                if tx.send(Ev::Bus(m)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Ev::BusGone);
        });
    }
    {
        let (tx, wl_rx) = (tx.clone(), wl.take_receiver());
        std::thread::spawn(move || {
            for m in wl_rx {
                if tx.send(Ev::Wl(m)).is_err() {
                    return;
                }
            }
        });
    }
    spawn_media(tx.clone());
    let cmds = spawn_commands(conn.clone());

    let mut app = App {
        svc: Service::new(conn),
        wl,
        fonts,
        tx,
        cmds,
        images: Images::default(),
        media: None,
        panel: None,
        toast: None,
        premute: 0.5,
        fatal: None,
    };
    app.event_loop(rx)
}

fn spawn_media(tx: Sender<Ev>) {
    std::thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        loop {
            if let Ok(mut c) = mpris::Client::new() {
                let _ = c.subscribe();
                backoff = Duration::from_secs(1);
                loop {
                    match c.current() {
                        Ok(s) => {
                            if tx.send(Ev::Media(s)).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                    if c.wait_for_change(Duration::from_secs(4)).is_err() {
                        break;
                    }
                }
            }
            if tx.send(Ev::Media(None)).is_err() {
                return;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

/// Player commands run here so a stuck player cannot freeze the panel. A
/// burst of volume changes (dragging the slider) collapses into the last one.
fn spawn_commands(conn: Conn) -> Sender<MediaCmd> {
    let (tx, rx) = channel::<MediaCmd>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            while let Ok(c) = rx.try_recv() {
                batch.push(c);
            }
            let mut skip_volume = batch.iter().rposition(|c| matches!(c, MediaCmd::Volume(..)));
            for (i, c) in batch.into_iter().enumerate() {
                let _ = match c {
                    MediaCmd::PlayPause(b) => mpris::play_pause(&conn, &b),
                    MediaCmd::Next(b) => mpris::next(&conn, &b),
                    MediaCmd::Prev(b) => mpris::previous(&conn, &b),
                    MediaCmd::Volume(b, v) => {
                        if skip_volume == Some(i) {
                            skip_volume = None;
                            mpris::set_volume(&conn, &b, v)
                        } else {
                            Ok(())
                        }
                    }
                    MediaCmd::Seek { bus, track, target_us, current_us } => mpris::seek_to(&conn, &bus, track.as_deref(), target_us, current_us),
                };
            }
        }
    });
    tx
}

fn frac_of(r: Option<crate::ui::paint::R>, x: f32) -> f64 {
    match r {
        Some(r) if r.w > 0.0 => (((x - r.x) / r.w) as f64).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

impl App {
    fn event_loop(&mut self, rx: Receiver<Ev>) -> Result<(), String> {
        loop {
            let now = Instant::now();
            let (expired, next_expiry) = self.svc.tick(now);
            if expired {
                self.dirty_toast();
            }
            // keep a hovered toast alive
            let hovering = self.toast.as_ref().and_then(|t| match &t.hover {
                Some(HitId::Item(id)) | Some(HitId::Close(id)) | Some(HitId::Action(id, _)) => Some(*id),
                _ => None,
            });
            if let Some(id) = hovering {
                self.svc.center.postpone_toast(id, now);
            }
            self.flush();
            if let Some(e) = self.fatal.take() {
                return Err(e);
            }

            let mut wake = next_expiry.map(|t| t.saturating_duration_since(now)).unwrap_or(Duration::from_secs(3600));
            if hovering.is_some() {
                wake = wake.min(Duration::from_millis(500));
            }
            if self.panel.is_some() {
                if let Some(m) = &self.media {
                    if m.snap.status == Status::Playing {
                        let us = m.clock.now_us(m.snap.track.length_us);
                        wake = wake.min(Duration::from_micros((1_000_000 - us.rem_euclid(1_000_000)) as u64 + 5_000));
                    }
                }
            }
            match rx.recv_timeout(wake) {
                Ok(ev) => {
                    self.on_event(ev);
                    let mut n = 0;
                    while let Ok(ev) = rx.try_recv() {
                        self.on_event(ev);
                        n += 1;
                        if n > 512 || self.fatal.is_some() {
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // the playback clock moved on
                    if let Some(p) = &mut self.panel {
                        p.dirty = true;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err("internal event channel closed".into()),
            }
            if let Some(e) = self.fatal.take() {
                return Err(e);
            }
        }
    }

    fn dirty_toast(&mut self) {
        if let Some(t) = &mut self.toast {
            t.dirty = true;
        }
    }

    fn dirty_all(&mut self) {
        if let Some(p) = &mut self.panel {
            p.dirty = true;
        }
        if let Some(t) = &mut self.toast {
            t.dirty = true;
        }
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::BusGone => self.fatal = Some("lost the session bus".into()),
            Ev::Bus(m) => {
                let out = self.svc.handle(&m, Instant::now());
                if out.changed {
                    self.dirty_all();
                }
                match out.panel {
                    Some(PanelCmd::Toggle) => {
                        if self.panel.is_some() {
                            self.close_panel()
                        } else {
                            self.open_panel()
                        }
                    }
                    Some(PanelCmd::Open) => {
                        if self.panel.is_none() {
                            self.open_panel()
                        }
                    }
                    Some(PanelCmd::Close) => self.close_panel(),
                    None => {}
                }
            }
            Ev::Media(snap) => self.on_media(snap),
            Ev::Image(key, dev, px) => {
                self.images.map.insert((key, dev), px.map(Slot::Ready).unwrap_or(Slot::Failed));
                self.dirty_all();
            }
            Ev::Wl(inc) => {
                for e in self.wl.handle(inc) {
                    self.on_wl(e);
                }
            }
        }
    }

    fn on_media(&mut self, snap: Option<Snapshot>) {
        let changed = match (&self.media, &snap) {
            (None, None) => false,
            (Some(a), Some(b)) => {
                a.snap.bus_name != b.bus_name
                    || a.snap.status != b.status
                    || a.snap.track != b.track
                    || (a.snap.volume - b.volume).abs() > 0.001
                    || a.snap.can_seek != b.can_seek
                    || (a.clock.now_us(b.track.length_us) - b.position_us).abs() > 1_500_000
            }
            _ => true,
        };
        if !changed {
            return;
        }
        self.media = snap.map(|s| MediaState { clock: PositionClock::new(s.position_us, s.status == Status::Playing), snap: s });
        if let Some(p) = &mut self.panel {
            p.dirty = true;
        }
    }

    // ------------------------------------------------------------ panel life cycle

    fn open_panel(&mut self) {
        let scrim = LayerOpts {
            layer: LAYER_TOP,
            anchor: ANCHOR_TOP | ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT,
            width: 0,
            height: 0,
            margin: [0; 4],
            zone: -1,
            namespace: "notify-rs-scrim".into(),
            blank: true,
        };
        let panel = LayerOpts {
            layer: LAYER_OVERLAY,
            anchor: ANCHOR_TOP,
            width: panel::surface_w() as u32,
            height: 420,
            margin: [0; 4],
            zone: 0,
            namespace: "notify-rs".into(),
            blank: false,
        };
        let sids = self.wl.create_layer(scrim).and_then(|s| self.wl.create_layer(panel).map(|p| (s, p)));
        match sids {
            Ok((scrim, sid)) => {
                self.panel = Some(PanelWin {
                    sid,
                    scrim,
                    frame: Frame::default(),
                    hover: None,
                    pressed: None,
                    drag: None,
                    scroll: 0.0,
                    dirty: true,
                    last_second: -1,
                });
                self.svc.set_panel_open(true);
                self.close_toasts();
            }
            Err(e) => self.fatal = Some(e),
        }
    }

    fn close_panel(&mut self) {
        if let Some(p) = self.panel.take() {
            let _ = self.wl.destroy(p.sid);
            let _ = self.wl.destroy(p.scrim);
        }
        self.svc.set_panel_open(false);
    }

    fn close_toasts(&mut self) {
        if let Some(t) = self.toast.take() {
            let _ = self.wl.destroy(t.sid);
        }
    }

    // ------------------------------------------------------------ wayland events

    fn on_wl(&mut self, e: Event) {
        match e {
            Event::Disconnected(m) => self.fatal = Some(format!("lost the Wayland connection: {m}")),
            Event::Configure { sid, .. } | Event::ScaleChanged(sid) | Event::Redraw(sid) => {
                if let Some(p) = self.panel.as_mut().filter(|p| p.sid == sid) {
                    p.dirty = true;
                }
                if let Some(t) = self.toast.as_mut().filter(|t| t.sid == sid) {
                    t.dirty = true;
                }
            }
            Event::Closed(sid) => {
                if self.panel.as_ref().is_some_and(|p| p.sid == sid || p.scrim == sid) {
                    self.close_panel();
                }
                if self.toast.as_ref().is_some_and(|t| t.sid == sid) {
                    self.toast = None;
                }
            }
            Event::PointerEnter { sid, x, y } | Event::PointerMotion { sid, x, y } => self.pointer_moved(sid, x, y),
            Event::PointerLeave { sid } => {
                if let Some(p) = self.panel.as_mut().filter(|p| p.sid == sid) {
                    if p.drag.is_none() && p.hover.is_some() {
                        p.hover = None;
                        p.dirty = true;
                    }
                }
                if let Some(t) = self.toast.as_mut().filter(|t| t.sid == sid) {
                    if t.hover.is_some() {
                        t.hover = None;
                        t.dirty = true;
                    }
                }
            }
            Event::PointerButton { sid, x, y, button, pressed } => self.pointer_button(sid, x, y, button, pressed),
            Event::Scroll { sid, dy } => self.scrolled(sid, dy),
        }
    }

    fn bus(&self) -> Option<String> {
        self.media.as_ref().map(|m| m.snap.bus_name.clone())
    }

    fn send(&self, c: MediaCmd) {
        let _ = self.cmds.send(c);
    }

    fn cursor_for(&self, id: &Option<HitId>) -> u32 {
        match id {
            Some(HitId::Item(i)) => {
                if self.svc.center.get(*i).is_some_and(|n| n.has_default_action()) {
                    SHAPE_POINTER
                } else {
                    SHAPE_DEFAULT
                }
            }
            Some(h) if h.clickable() => SHAPE_POINTER,
            _ => SHAPE_DEFAULT,
        }
    }

    fn pointer_moved(&mut self, sid: usize, x: f32, y: f32) {
        if self.panel.as_ref().is_some_and(|p| p.sid == sid) {
            let mut volume_cmd = None;
            let hit = {
                let p = self.panel.as_mut().unwrap();
                if let Some((id, v)) = &mut p.drag {
                    let r = if *id == HitId::Seek { p.frame.seek } else { p.frame.volume };
                    *v = frac_of(r, x);
                    if *id == HitId::Volume {
                        volume_cmd = Some(*v);
                    }
                    p.dirty = true;
                }
                let hit = p.frame.hit(x, y).map(|h| h.id.clone()).filter(|id| *id != HitId::List);
                if hit != p.hover {
                    p.hover = hit.clone();
                    p.dirty = true;
                }
                hit
            };
            let cursor = if self.panel.as_ref().is_some_and(|p| p.drag.is_some()) { SHAPE_POINTER } else { self.cursor_for(&hit) };
            if let (Some(v), Some(bus)) = (volume_cmd, self.bus()) {
                if let Some(m) = &mut self.media {
                    m.snap.volume = v;
                }
                self.send(MediaCmd::Volume(bus, v));
            }
            self.wl.set_cursor(cursor);
        } else if self.panel.as_ref().is_some_and(|p| p.scrim == sid) {
            if let Some(p) = &mut self.panel {
                if p.hover.is_some() && p.drag.is_none() {
                    p.hover = None;
                    p.dirty = true;
                }
            }
            self.wl.set_cursor(SHAPE_DEFAULT);
        } else if self.toast.as_ref().is_some_and(|t| t.sid == sid) {
            let hit = {
                let t = self.toast.as_mut().unwrap();
                let hit = t.frame.hit(x, y).map(|h| h.id.clone());
                if hit != t.hover {
                    t.hover = hit.clone();
                    t.dirty = true;
                }
                hit
            };
            let cursor = self.cursor_for(&hit);
            self.wl.set_cursor(cursor);
        }
    }

    fn pointer_button(&mut self, sid: usize, x: f32, y: f32, button: u32, pressed: bool) {
        if self.panel.as_ref().is_some_and(|p| p.scrim == sid) {
            if pressed {
                self.close_panel(); // a click anywhere outside the card closes it
            }
            return;
        }
        if self.panel.as_ref().is_some_and(|p| p.sid == sid) {
            self.panel_button(x, y, button, pressed);
        } else if self.toast.as_ref().is_some_and(|t| t.sid == sid) {
            self.toast_button(x, y, button, pressed);
        }
    }

    fn panel_button(&mut self, x: f32, y: f32, button: u32, pressed: bool) {
        let hit = self.panel.as_ref().and_then(|p| p.frame.hit(x, y).map(|h| h.id.clone()));
        if pressed {
            if button == BTN_MIDDLE {
                if let Some(HitId::Item(id)) | Some(HitId::Close(id)) = hit {
                    self.svc.dismiss(id);
                    self.dirty_all();
                }
                return;
            }
            if button != BTN_LEFT {
                return;
            }
            let mut start_volume = None;
            if let Some(p) = &mut self.panel {
                p.pressed = hit.clone();
                match hit {
                    Some(HitId::Seek) => p.drag = Some((HitId::Seek, frac_of(p.frame.seek, x))),
                    Some(HitId::Volume) => {
                        let v = frac_of(p.frame.volume, x);
                        p.drag = Some((HitId::Volume, v));
                        start_volume = Some(v);
                    }
                    _ => {}
                }
                p.dirty = true;
            }
            if let (Some(v), Some(bus)) = (start_volume, self.bus()) {
                if let Some(m) = &mut self.media {
                    m.snap.volume = v;
                }
                self.send(MediaCmd::Volume(bus, v));
            }
            return;
        }
        if button != BTN_LEFT {
            return;
        }
        // release
        let (pressed_on, drag) = match &mut self.panel {
            Some(p) => {
                p.dirty = true;
                (p.pressed.take(), p.drag.take())
            }
            None => return,
        };
        if let Some((HitId::Seek, f)) = drag {
            if let (Some(bus), Some(m)) = (self.bus(), &mut self.media) {
                let length = m.snap.track.length_us;
                let target = (f * length as f64) as i64;
                let current = m.clock.now_us(length);
                let track = m.snap.track.track_id.clone();
                m.clock = PositionClock::new(target, m.snap.status == Status::Playing);
                self.send(MediaCmd::Seek { bus, track, target_us: target, current_us: current });
            }
            return;
        }
        if drag.is_some() {
            return;
        }
        if pressed_on.is_some() && pressed_on == hit {
            self.perform(hit.unwrap(), false);
        }
    }

    fn toast_button(&mut self, x: f32, y: f32, button: u32, pressed: bool) {
        let hit = self.toast.as_ref().and_then(|t| t.frame.hit(x, y).map(|h| h.id.clone()));
        if button != BTN_LEFT {
            return;
        }
        if pressed {
            if let Some(t) = &mut self.toast {
                t.pressed = hit;
                t.dirty = true;
            }
            return;
        }
        let pressed_on = self.toast.as_mut().and_then(|t| {
            t.dirty = true;
            t.pressed.take()
        });
        if pressed_on.is_some() && pressed_on == hit {
            self.perform(hit.unwrap(), true);
        }
    }

    /// A button was clicked. `toast`: it was on a banner rather than in the panel.
    fn perform(&mut self, id: HitId, toast: bool) {
        match id {
            HitId::PlayPause => {
                if let Some(b) = self.bus() {
                    self.send(MediaCmd::PlayPause(b));
                }
            }
            HitId::Next => {
                if let Some(b) = self.bus() {
                    self.send(MediaCmd::Next(b));
                }
            }
            HitId::Prev => {
                if let Some(b) = self.bus() {
                    self.send(MediaCmd::Prev(b));
                }
            }
            HitId::Mute => {
                if let (Some(b), Some(m)) = (self.bus(), &mut self.media) {
                    let v = if m.snap.volume > 0.001 {
                        self.premute = m.snap.volume;
                        0.0
                    } else {
                        self.premute.max(0.1)
                    };
                    m.snap.volume = v;
                    self.send(MediaCmd::Volume(b, v));
                }
            }
            HitId::Dnd => {
                let on = !self.svc.center.dnd();
                self.svc.set_dnd(on);
            }
            HitId::Clear => self.svc.clear_all(),
            HitId::Item(n) => {
                if self.svc.center.get(n).is_some_and(|x| x.has_default_action()) {
                    self.svc.activate(n, "default");
                } else if toast {
                    self.svc.center.hide_toast(n);
                }
            }
            HitId::Close(n) => {
                if toast {
                    self.svc.center.hide_toast(n);
                } else {
                    self.svc.dismiss(n);
                }
            }
            HitId::Action(n, key) => self.svc.activate(n, &key),
            HitId::Seek | HitId::Volume | HitId::List => {}
        }
        self.dirty_all();
    }

    fn scrolled(&mut self, sid: usize, dy: f32) {
        let Some(p) = self.panel.as_mut().filter(|p| p.sid == sid) else { return };
        if matches!(p.hover, Some(HitId::Volume) | Some(HitId::Mute)) {
            let delta = -(dy as f64 / 15.0) * 0.05;
            if let (Some(bus), Some(m)) = (self.media.as_ref().map(|m| m.snap.bus_name.clone()), &mut self.media) {
                let v = (m.snap.volume + delta).clamp(0.0, 1.0);
                m.snap.volume = v;
                p.dirty = true;
                let _ = self.cmds.send(MediaCmd::Volume(bus, v));
            }
        } else {
            p.scroll = (p.scroll + dy * 2.5).max(0.0);
            p.dirty = true; // the draw pass clamps it to the content
        }
    }

    // ------------------------------------------------------------ rendering

    fn flush(&mut self) {
        // a bug in drawing must not take the notification daemon down with it
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.sync_toasts();
            self.render_panel();
            self.render_toasts();
        }));
        if r.is_err() {
            eprintln!("notify-rs: internal error while drawing; closing the panel");
            self.close_panel();
            self.close_toasts();
        }
        if let Some(d) = self.wl.is_dead() {
            self.fatal.get_or_insert_with(|| format!("lost the Wayland connection: {d}"));
        }
    }

    /// Prepare every picture the visible notifications want at `dev`-pixel size.
    fn load_images(&mut self, items: &[&Notification], dev: usize) {
        for n in items {
            if let (Some(img), Some(key)) = (&n.image, note_source(n)) {
                self.images.want_raw(&key, dev, img);
            } else if let Some(src) = note_source(n) {
                self.images.want(&src, dev, &self.tx);
            }
        }
    }

    fn media_view(&mut self, dev: usize) -> Option<MediaView> {
        let m = self.media.as_ref()?;
        let url = m.snap.track.art_url.clone();
        let art = url.as_deref().and_then(|u| self.images.get(u, dev).map(|px| Art { dev, px }));
        let length = m.snap.track.length_us;
        let view = MediaView {
            identity: m.snap.identity.clone(),
            playing: m.snap.status == Status::Playing,
            title: m.snap.track.title.clone(),
            artist: m.snap.track.artist_line(),
            album: m.snap.track.album.clone(),
            position_us: m.clock.now_us(length),
            length_us: length,
            volume: m.snap.volume.clamp(0.0, 1.0),
            can_seek: m.snap.can_seek,
            art,
        };
        if art_missing(&view, url.as_deref(), &self.images, dev) {
            if let Some(u) = url.filter(|u| art::loadable(u)) {
                self.images.want(&u, dev, &self.tx);
            }
        }
        Some(view)
    }

    fn render_panel(&mut self) {
        let Some(sid) = self.panel.as_ref().filter(|p| p.dirty).map(|p| p.sid) else { return };
        let Some((lw, lh)) = self.wl.logical_size(sid) else { return }; // not configured yet
        let scale = self.wl.scale(sid);
        let Some((bw, bh)) = self.wl.buffer_size(sid) else { return };
        self.panel.as_mut().unwrap().dirty = false;

        let out_h = self.panel.as_ref().and_then(|p| self.wl.logical_size(p.scrim)).map(|s| s.1 as f32).unwrap_or(900.0);
        let max_card_h = (out_h - 2.0 * MARGIN - 70.0).max(320.0);

        let dev76 = (76.0 * scale).round() as usize;
        let dev40 = (40.0 * scale).round() as usize;
        let media = self.media_view(dev76);
        let items: Vec<&Notification> = self.svc.center.items_newest_first().collect();
        // pictures first (needs &mut images), then draw with a shared view of the cache
        let owned: Vec<Notification> = items.iter().map(|n| (*n).clone()).collect();
        let refs: Vec<&Notification> = owned.iter().collect();
        self.load_images(&refs, dev40);
        let images = &self.images;
        let lookup = |n: &Notification, dev: usize| note_source(n).and_then(|k| images.get(&k, dev));

        let p = self.panel.as_mut().unwrap();
        let state = PanelState {
            media: media.as_ref(),
            items: &refs,
            dnd: self.svc.center.dnd(),
            now_unix: service::now_unix(),
            hover: p.hover.as_ref(),
            pressed: p.pressed.as_ref(),
            drag: p.drag.clone(),
            scroll: p.scroll,
            max_card_h,
            images: &lookup,
        };
        let measured = panel::draw_panel(&mut Painter::new(None, scale, &mut self.fonts), &state);
        let want_h = measured.h.ceil() as u32;
        if want_h != lh || panel::surface_w() as u32 != lw {
            let _ = self.wl.resize(sid, panel::surface_w() as u32, want_h);
            p.dirty = false; // the Configure that follows draws it
            return;
        }
        let mut cv = Canvas::new(bw as usize, bh as usize);
        let frame = panel::draw_panel(&mut Painter::new(Some(&mut cv), scale, &mut self.fonts), &state);
        p.scroll = frame.scroll;
        p.last_second = media.as_ref().map(|m| m.position_us / 1_000_000).unwrap_or(-1);
        let card = frame.card;
        p.frame = frame;
        match self.wl.present(sid, &cv) {
            Ok(true) | Ok(false) => {}
            Err(e) => {
                self.fatal = Some(e);
                return;
            }
        }
        if let Some(c) = card {
            let _ = self.wl.set_input_rect(sid, c.x.floor() as i32, c.y.floor() as i32, c.w.ceil() as i32, c.h.ceil() as i32);
        }
    }

    /// Create, resize or remove the toast window to match the visible toasts.
    fn sync_toasts(&mut self) {
        let want = !self.svc.center.toasts().is_empty() && self.panel.is_none();
        match (want, self.toast.is_some()) {
            (true, false) => {
                let opts = LayerOpts {
                    layer: LAYER_OVERLAY,
                    anchor: ANCHOR_TOP,
                    width: (panel::TOAST_W + 2.0 * MARGIN) as u32,
                    height: 120,
                    margin: [0; 4],
                    zone: 0,
                    namespace: "notify-rs-toasts".into(),
                    blank: false,
                };
                match self.wl.create_layer(opts) {
                    Ok(sid) => self.toast = Some(ToastWin { sid, frame: Frame::default(), hover: None, pressed: None, dirty: true }),
                    Err(e) => self.fatal = Some(e),
                }
            }
            (false, true) => self.close_toasts(),
            _ => {}
        }
    }

    fn render_toasts(&mut self) {
        let Some(sid) = self.toast.as_ref().filter(|t| t.dirty).map(|t| t.sid) else { return };
        let Some((lw, lh)) = self.wl.logical_size(sid) else { return };
        let scale = self.wl.scale(sid);
        let Some((bw, bh)) = self.wl.buffer_size(sid) else { return };
        self.toast.as_mut().unwrap().dirty = false;

        let owned: Vec<Notification> = self.svc.center.toasts().into_iter().cloned().collect();
        if owned.is_empty() {
            return;
        }
        let refs: Vec<&Notification> = owned.iter().collect();
        self.load_images(&refs, (40.0 * scale).round() as usize);
        let images = &self.images;
        let lookup = |n: &Notification, dev: usize| note_source(n).and_then(|k| images.get(&k, dev));
        let t = self.toast.as_mut().unwrap();
        let now = service::now_unix();
        let measured = panel::draw_toasts(&mut Painter::new(None, scale, &mut self.fonts), &refs, now, t.hover.as_ref(), t.pressed.as_ref(), &lookup);
        let want_h = measured.h.ceil() as u32;
        if want_h != lh || (panel::TOAST_W + 2.0 * MARGIN) as u32 != lw {
            let _ = self.wl.resize(sid, (panel::TOAST_W + 2.0 * MARGIN) as u32, want_h);
            return;
        }
        let mut cv = Canvas::new(bw as usize, bh as usize);
        let frame = panel::draw_toasts(&mut Painter::new(Some(&mut cv), scale, &mut self.fonts), &refs, now, t.hover.as_ref(), t.pressed.as_ref(), &lookup);
        let card = frame.card;
        t.frame = frame;
        if let Err(e) = self.wl.present(sid, &cv) {
            self.fatal = Some(e);
            return;
        }
        if let Some(c) = card {
            let _ = self.wl.set_input_rect(sid, c.x.floor() as i32, c.y.floor() as i32, c.w.ceil() as i32, c.h.ceil() as i32);
        }
    }
}

fn art_missing(view: &MediaView, url: Option<&str>, images: &Images, dev: usize) -> bool {
    view.art.is_none() && url.is_some_and(|u| !images.map.contains_key(&(u.to_string(), dev)))
}
