//! The Wayland client against tools/mock_compositor.py: real sockets, real
//! SCM_RIGHTS file descriptors, real shared memory. The mock is strict about
//! protocol order, ids and buffer sizes, so `assert_clean` is part of every test.

mod common;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use common::{kv, Mock};
use notify_rs::gfx::canvas::{Canvas, Color, Rect};
use notify_rs::wl::client::*;
use notify_rs::wl::Incoming;

const W: i32 = 1920;

fn panel_opts() -> LayerOpts {
    LayerOpts { layer: LAYER_OVERLAY, anchor: ANCHOR_TOP, width: 400, height: 300, margin: [8, 0, 0, 0], zone: 0, namespace: "panel".into(), blank: false }
}

fn scrim_opts() -> LayerOpts {
    LayerOpts {
        layer: LAYER_TOP,
        anchor: ANCHOR_TOP | ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT,
        width: 0,
        height: 0,
        margin: [0; 4],
        zone: -1,
        namespace: "scrim".into(),
        blank: true,
    }
}

struct Rig {
    mock: Mock,
    wl: Wl,
    rx: Receiver<Incoming>,
    events: Vec<Event>,
}

fn rig(args: &[&str]) -> Option<Rig> {
    let mock = Mock::start(args)?;
    let mut wl = Wl::connect_at(&mock.socket).unwrap_or_else(|e| panic!("connect: {e}"));
    let rx = wl.take_receiver();
    Some(Rig { mock, wl, rx, events: Vec::new() })
}

impl Rig {
    /// Handle incoming messages for up to `ms`, recording the events.
    fn pump_for(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(inc) = self.rx.recv_timeout(left.min(Duration::from_millis(20))) {
                let ev = self.wl.handle(inc);
                self.events.extend(ev);
            }
        }
    }

    /// Pump until an event satisfies `f`; returns it.
    fn until(&mut self, what: &str, f: impl Fn(&Event) -> bool) -> Event {
        let end = Instant::now() + Duration::from_secs(5);
        let mut scanned = 0;
        loop {
            while scanned < self.events.len() {
                scanned += 1;
                if f(&self.events[scanned - 1]) {
                    return self.events.remove(scanned - 1);
                }
            }
            if Instant::now() > end {
                panic!("timed out waiting for {what}; events: {:?}\nmock:\n{}", self.events, self.mock.lines().join("\n"));
            }
            if let Ok(inc) = self.rx.recv_timeout(Duration::from_millis(20)) {
                let ev = self.wl.handle(inc);
                self.events.extend(ev);
            }
        }
    }

    fn open(&mut self, o: LayerOpts) -> usize {
        let sid = self.wl.create_layer(o).unwrap();
        self.until("configure", |e| matches!(e, Event::Configure { sid: s, .. } if *s == sid));
        sid
    }

    /// Draw a solid frame at the current buffer size and wait for the compositor to take it.
    fn show(&mut self, sid: usize, color: Color) -> String {
        let (bw, bh) = self.wl.buffer_size(sid).unwrap();
        let mut cv = Canvas::new(bw as usize, bh as usize);
        cv.clear(color);
        let from = self.mock.pos();
        assert!(self.wl.present(sid, &cv).unwrap(), "no free buffer");
        self.mock.wait(from, "a commit", |l| l.starts_with("commit") && kv(l, "buffer") == Some(&format!("{bw}x{bh}")))
    }
}

fn png_pixel(commit: &str, x: usize, y: usize) -> [u8; 4] {
    let path = kv(commit, "png").expect("png dump");
    let img = notify_rs::image::decode(&std::fs::read(path).unwrap()).unwrap();
    let i = (y * img.w + x) * 4;
    [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
}

#[test]
fn binds_what_it_needs() {
    let Some(mut r) = rig(&[]) else { return };
    for want in [
        "bind interface=wl_compositor version=6",
        "bind interface=wl_shm version=1",
        "bind interface=wl_seat version=5",
        "bind interface=wl_output version=3",
        "bind interface=zwlr_layer_shell_v1 version=4",
        "bind interface=wp_viewporter version=1",
        "bind interface=wp_fractional_scale_manager_v1 version=1",
        "bind interface=wp_cursor_shape_manager_v1 version=1",
    ] {
        r.mock.wait(0, want, |l| l == want);
    }
    assert!(r.wl.has_fractional_scale() && r.wl.has_cursor_shapes());
    r.mock.assert_clean();
}

#[test]
fn panel_gets_configured_and_shows_pixels() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    assert_eq!(r.wl.logical_size(sid), Some((400, 300)));
    assert_eq!(r.wl.buffer_size(sid), Some((400, 300)));
    let c = r.show(sid, Color::rgb(200, 10, 20));
    assert_eq!(kv(&c, "viewport"), Some("400x300"), "fractional-capable compositors get a viewport destination");
    assert_eq!(kv(&c, "scale"), Some("1"));
    assert_eq!(kv(&c, "namespace"), Some("panel"));
    assert_eq!(png_pixel(&c, 5, 5), [200, 10, 20, 255]);
    assert_eq!(png_pixel(&c, 399, 299), [200, 10, 20, 255]);
    let log = r.mock.lines().join("\n");
    assert!(log.contains("layer_surface namespace=panel layer=3"), "{log}");
    assert!(log.contains("zone namespace=panel zone=0"), "{log}");
    assert!(log.contains("keyboard namespace=panel mode=0"), "never steal keyboard focus: {log}");
    r.mock.assert_clean();
}

#[test]
fn transparent_pixels_survive_premultiplication() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    let mut cv = Canvas::new(400, 300);
    cv.fill_rect(Rect::new(0, 0, 400, 300), Color::rgba(0, 0, 0, 0));
    cv.fill_rect(Rect::new(10, 10, 50, 50), Color::rgba(255, 128, 0, 255));
    let from = r.mock.pos();
    assert!(r.wl.present(sid, &cv).unwrap());
    let c = r.mock.wait(from, "commit", |l| l.starts_with("commit"));
    assert_eq!(png_pixel(&c, 100, 100)[3], 0, "outside the card is fully transparent");
    assert_eq!(png_pixel(&c, 20, 20), [255, 128, 0, 255]);
    r.mock.assert_clean();
}

#[test]
fn fractional_scale_changes_buffer_size_but_not_logical_size() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    r.show(sid, Color::rgb(1, 2, 3));
    r.mock.cmd("scale 150");
    r.until("scale change", |e| *e == Event::ScaleChanged(sid));
    assert!((r.wl.scale(sid) - 1.25).abs() < 1e-6);
    assert_eq!(r.wl.buffer_size(sid), Some((500, 375)));
    let c = r.show(sid, Color::rgb(9, 9, 9));
    assert_eq!((kv(&c, "viewport"), kv(&c, "scale"), kv(&c, "logical")), (Some("400x300"), Some("1"), Some("400x300")));
    r.mock.cmd("scale 195");
    r.until("scale change", |e| *e == Event::ScaleChanged(sid));
    assert_eq!(r.wl.buffer_size(sid), Some((650, 488)), "rounded to the nearest pixel, as the protocol says");
    r.show(sid, Color::rgb(9, 9, 9));
    // a wrongly sized canvas is refused rather than sent
    assert!(r.wl.present(sid, &Canvas::new(400, 300)).is_err());
    r.mock.assert_clean();
}

#[test]
fn whole_scales_use_buffer_scale_when_there_is_no_viewporter() {
    let Some(mut r) = rig(&["--without", "viewporter,fractional"]) else { return };
    assert!(!r.wl.has_fractional_scale());
    let sid = r.open(panel_opts());
    let c = r.show(sid, Color::rgb(1, 2, 3));
    assert_eq!((kv(&c, "scale"), kv(&c, "viewport")), (Some("1"), Some("-")));
    r.mock.cmd("scale 240");
    r.until("scale change", |e| *e == Event::ScaleChanged(sid));
    assert_eq!(r.wl.buffer_size(sid), Some((800, 600)));
    let c = r.show(sid, Color::rgb(1, 2, 3));
    assert_eq!((kv(&c, "scale"), kv(&c, "logical")), (Some("2"), Some("400x300")));
    // 1.25 can only be shown at 2x without a viewport
    r.mock.cmd("scale 150");
    r.pump_for(200);
    assert_eq!(r.wl.buffer_size(sid), Some((800, 600)));
    r.mock.assert_clean();
}

#[test]
fn buffers_are_reused_and_never_overwritten_while_held() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    for i in 0..8u8 {
        r.show(sid, Color::rgb(i * 10, 0, 0));
        r.pump_for(60); // let the releases arrive
    }
    let made = r.mock.lines().iter().filter(|l| l.starts_with("buffer ")).count();
    assert!(made <= 3, "{made} buffers were created for a steady stream of frames");

    // hold everything: after three buffers the client must say "not now" instead of drawing into one in use
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    r.mock.cmd("hold on");
    for i in 0..3u8 {
        r.show(sid, Color::rgb(i, 0, 0));
        r.pump_for(30);
    }
    let (bw, bh) = r.wl.buffer_size(sid).unwrap();
    assert_eq!(r.wl.present(sid, &Canvas::new(bw as usize, bh as usize)).unwrap(), false, "all buffers are held");
    r.mock.cmd("release");
    r.until("redraw", |e| *e == Event::Redraw(sid));
    assert!(r.wl.present(sid, &Canvas::new(bw as usize, bh as usize)).unwrap());
    r.mock.assert_clean();
}

#[test]
fn resizing_asks_for_a_configure_and_changes_the_buffer() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    r.show(sid, Color::rgb(1, 1, 1));
    r.wl.resize(sid, 400, 520).unwrap();
    let e = r.until("configure", |e| matches!(e, Event::Configure { .. }));
    assert_eq!(e, Event::Configure { sid, w: 400, h: 520 });
    assert_eq!(r.wl.buffer_size(sid), Some((400, 520)));
    let c = r.show(sid, Color::rgb(1, 1, 1));
    assert_eq!(kv(&c, "logical"), Some("400x520"));
    r.pump_for(100);
    r.mock.assert_clean();
}

#[test]
fn blank_scrim_is_a_one_pixel_buffer_stretched_by_the_viewport() {
    let Some(mut r) = rig(&[]) else { return };
    let from = r.mock.pos();
    let _sid = r.wl.create_layer(scrim_opts()).unwrap();
    r.pump_for(300);
    let c = r.mock.wait(from, "scrim commit", |l| l.starts_with("commit") && kv(l, "namespace") == Some("scrim"));
    assert_eq!((kv(&c, "buffer"), kv(&c, "viewport")), (Some("1x1"), Some("1920x1080")));
    assert!(r.mock.lines().join("\n").contains("zone namespace=scrim zone=-1"));
    r.mock.assert_clean();

    let Some(mut r) = rig(&["--without", "viewporter,fractional"]) else { return };
    let from = r.mock.pos();
    let _sid = r.wl.create_layer(scrim_opts()).unwrap();
    r.pump_for(300);
    let c = r.mock.wait(from, "scrim commit", |l| l.starts_with("commit") && kv(l, "namespace") == Some("scrim"));
    assert_eq!((kv(&c, "buffer"), kv(&c, "viewport")), (Some("1920x1080"), Some("-")), "without a viewporter it needs a full-size buffer");
    r.mock.assert_clean();
}

fn pointer_rig(args: &[&str]) -> Option<(Rig, usize, usize)> {
    let mut r = rig(args)?;
    let from = r.mock.pos();
    let scrim = r.wl.create_layer(scrim_opts()).unwrap();
    r.pump_for(300);
    r.mock.wait(from, "the scrim to be shown", |l| l.starts_with("commit") && kv(l, "namespace") == Some("scrim"));
    let panel = r.open(panel_opts());
    r.show(panel, Color::rgb(30, 30, 30));
    r.pump_for(100);
    Some((r, panel, scrim))
}

#[test]
fn pointer_events_arrive_in_surface_coordinates_and_the_topmost_surface_wins() {
    let Some((mut r, panel, scrim)) = pointer_rig(&[]) else { return };
    // the panel is centred: x 760..1160, y 8..308
    r.mock.cmd("click 800 100");
    let e = r.until("enter", |e| matches!(e, Event::PointerEnter { .. }));
    assert_eq!(e, Event::PointerEnter { sid: panel, x: 40.0, y: 92.0 });
    let e = r.until("press", |e| matches!(e, Event::PointerButton { pressed: true, .. }));
    assert_eq!(e, Event::PointerButton { sid: panel, x: 40.0, y: 92.0, button: BTN_LEFT, pressed: true });
    r.until("release", |e| matches!(e, Event::PointerButton { pressed: false, .. }));

    r.mock.cmd("click 100 500");
    r.until("leave panel", |e| *e == Event::PointerLeave { sid: panel });
    let e = r.until("enter scrim", |e| matches!(e, Event::PointerEnter { .. }));
    assert_eq!(e, Event::PointerEnter { sid: scrim, x: 100.0, y: 500.0 });
    let e = r.until("click on scrim", |e| matches!(e, Event::PointerButton { pressed: true, .. }));
    assert!(matches!(e, Event::PointerButton { sid, .. } if sid == scrim));
    r.mock.assert_clean();
    let _ = W;
}

#[test]
fn input_region_lets_clicks_through_the_transparent_margin() {
    let Some((mut r, panel, scrim)) = pointer_rig(&[]) else { return };
    r.wl.set_input_rect(panel, 20, 20, 360, 260).unwrap();
    r.pump_for(100);
    r.mock.cmd("move 770 15"); // inside the panel surface (10, 7) but outside the input rectangle
    let e = r.until("enter", |e| matches!(e, Event::PointerEnter { .. }));
    assert!(matches!(e, Event::PointerEnter { sid, .. } if sid == scrim), "{e:?}");
    r.mock.cmd("move 800 100");
    let e = r.until("enter panel", |e| matches!(e, Event::PointerEnter { sid, .. } if *sid == panel));
    assert_eq!(e, Event::PointerEnter { sid: panel, x: 40.0, y: 92.0 });
    r.mock.assert_clean();
}

#[test]
fn scrolling_and_cursor_shapes() {
    let Some((mut r, panel, _scrim)) = pointer_rig(&[]) else { return };
    r.mock.cmd("move 800 100");
    r.until("enter", |e| matches!(e, Event::PointerEnter { .. }));
    r.mock.cmd("axis 15");
    let e = r.until("scroll", |e| matches!(e, Event::Scroll { .. }));
    assert_eq!(e, Event::Scroll { sid: panel, dy: 15.0 });
    let from = r.mock.pos();
    r.wl.set_cursor(SHAPE_POINTER);
    let l = r.mock.wait(from, "cursor", |l| l.starts_with("cursor"));
    assert_eq!((kv(&l, "shape"), kv(&l, "valid")), (Some("4"), Some("true")), "{l}");
    let from = r.mock.pos();
    r.wl.set_cursor(SHAPE_POINTER); // same again: nothing to send
    r.wl.set_cursor(SHAPE_DEFAULT);
    let l = r.mock.wait(from, "cursor", |l| l.starts_with("cursor"));
    assert_eq!(kv(&l, "shape"), Some("1"));
    r.mock.assert_clean();
}

#[test]
fn a_seat_without_a_pointer_and_a_compositor_without_cursor_shapes_are_fine() {
    let Some(mut r) = rig(&["--no-pointer", "--without", "cursor"]) else { return };
    assert!(!r.wl.has_cursor_shapes());
    let sid = r.open(panel_opts());
    r.show(sid, Color::rgb(1, 2, 3));
    r.wl.set_cursor(SHAPE_POINTER); // must be a no-op
    r.pump_for(100);
    r.mock.assert_clean();
}

#[test]
fn object_ids_are_recycled_after_delete_id() {
    let Some(mut r) = rig(&[]) else { return };
    for _ in 0..40 {
        let sid = r.open(panel_opts());
        r.show(sid, Color::rgb(1, 2, 3));
        r.wl.destroy(sid).unwrap();
        r.pump_for(10);
    }
    let from = r.mock.pos();
    r.mock.cmd("state");
    let end = r.mock.wait(from, "state", |l| l.starts_with("state_end"));
    let max_id: u32 = kv(&end, "max_id").unwrap().parse().unwrap();
    let objects: u32 = kv(&end, "objects").unwrap().parse().unwrap();
    assert!(max_id < 60, "ids keep growing ({max_id}): delete_id is not recycled");
    assert!(objects < 30, "{objects} objects are still alive on the compositor side");
    r.mock.assert_clean(); // reusing an id too early would have been a protocol error
}

#[test]
fn the_compositor_closing_a_surface_is_reported() {
    let Some(mut r) = rig(&[]) else { return };
    let sid = r.open(panel_opts());
    r.show(sid, Color::rgb(1, 2, 3));
    r.mock.cmd("close panel");
    r.until("closed", |e| *e == Event::Closed(sid));
    assert!(r.wl.surface_ids().is_empty());
    assert!(r.wl.buffer_size(sid).is_none());
    r.mock.assert_clean();
}

#[test]
fn a_new_configure_size_is_passed_on() {
    let Some(mut r) = rig(&[]) else { return };
    let scrim = r.wl.create_layer(scrim_opts()).unwrap();
    r.pump_for(200);
    r.mock.cmd("configure scrim 2560 1440");
    r.pump_for(300);
    assert_eq!(r.wl.logical_size(scrim), Some((2560, 1440)));
    let c = r.mock.lines().into_iter().filter(|l| l.starts_with("commit")).last().unwrap();
    assert_eq!(kv(&c, "viewport"), Some("2560x1440"));
    r.mock.assert_clean();
}

#[test]
fn disconnects_and_errors_are_reported_not_hung_on() {
    let Some(mut r) = rig(&[]) else { return };
    r.mock.cmd("error");
    let e = r.until("error", |e| matches!(e, Event::Disconnected(_)));
    match e {
        Event::Disconnected(m) => assert!(m.contains("forced error") && m.contains("7"), "{m}"),
        _ => unreachable!(),
    }
    assert!(r.wl.is_dead().is_some());
    assert!(r.wl.create_layer(panel_opts()).is_err(), "a dead connection refuses work");

    let Some(mut r) = rig(&[]) else { return };
    r.mock.cmd("disconnect");
    let e = r.until("close", |e| matches!(e, Event::Disconnected(_)));
    assert!(matches!(e, Event::Disconnected(m) if m.contains("closed")));
}

#[test]
fn connection_failures_have_readable_messages() {
    let Some(mut m) = Mock::start(&["--without", "layer_shell"]) else { return };
    let err = Wl::connect_at(&m.socket).err().expect("must fail");
    assert!(err.contains("wlr-layer-shell") && err.contains("GNOME"), "{err}");
    drop(m.lines());
    let err = Wl::connect_at(std::path::Path::new("/nonexistent/wayland-9")).err().expect("must fail");
    assert!(err.contains("cannot connect to the Wayland compositor"), "{err}");
}
