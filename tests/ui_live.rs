//! The whole thing end to end: the real `notify-rs daemon` binary, a real
//! `dbus-daemon`, and the strict mock compositor. Notifications go in over
//! D-Bus; what comes out is checked as Wayland traffic, dumped pixels and the
//! D-Bus signals the applications would see. Clicks are sent from the mock at
//! coordinates computed with the same layout code the daemon uses.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use common::player::{Mock as Player, State};
use common::{kv, Mock};
use notify_rs::dbus::{Conn, Message, Value};
use notify_rs::gfx::text::Fonts;
use notify_rs::notify::Notification;
use notify_rs::service::{NOTIF_IFACE, NOTIF_NAME, NOTIF_PATH};
use notify_rs::ui::paint::Painter;
use notify_rs::ui::panel::{self, Frame, HitId, MediaView, PanelState};
use notify_rs::ui::preview::sample;

const OUT_H: f32 = 1080.0;

struct Surf {
    ns: String,
    layer: i32,
    anchor: i32,
    zone: i32,
    rect: [i32; 4],
    input: Option<[i32; 4]>,
}

struct Rig {
    bus: Child,
    address: String,
    mock: Mock,
    daemon: Option<Child>,
    client: Conn,
    _client_rx: Receiver<Message>,
    signals: Receiver<Message>,
    _listener: Conn,
    fonts: Fonts,
    scale: f32,
}

fn nums(s: &str) -> Option<Vec<i32>> {
    if s == "-" {
        return None;
    }
    s.split(',').map(|x| x.parse().ok()).collect()
}

impl Rig {
    /// Bus + compositor + a client connection; the daemon is started separately.
    fn start(mock_args: &[&str]) -> Option<Rig> {
        let fonts = Fonts::system().ok()?;
        let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut line = String::new();
        BufReader::new(bus.stdout.take()?).read_line(&mut line).ok()?;
        let address = line.trim().to_owned();
        let mock = Mock::start(mock_args)?;
        let (listener, signals) = Conn::connect(&address).unwrap();
        listener.add_match("type='signal',interface='org.freedesktop.Notifications'").unwrap();
        listener.add_match("type='signal',interface='dev.notifyrs.Daemon'").unwrap();
        let (client, client_rx) = Conn::connect(&address).unwrap();
        Some(Rig { bus, address, mock, daemon: None, client, _client_rx: client_rx, signals, _listener: listener, fonts, scale: 1.0 })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_notify-rs"));
        c.args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("XDG_RUNTIME_DIR", &self.mock.dir)
            .env("WAYLAND_DISPLAY", "wl-test")
            .env("HOME", &self.mock.dir);
        c
    }

    fn spawn_daemon(&mut self) -> Child {
        let log = std::fs::File::create(self.mock.dir.join("daemon.err")).unwrap();
        self.command(&["daemon"]).stderr(Stdio::from(log)).spawn().unwrap()
    }

    /// Start the daemon and wait until it answers on the bus.
    fn run_daemon(&mut self) {
        let child = self.spawn_daemon();
        self.daemon = Some(child);
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            if self.command(&["state"]).output().unwrap().status.success() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not come up:\n{}", self.daemon_log());
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.mock.dir.join("daemon.err")).unwrap_or_default()
    }

    fn cli_ok(&self, args: &[&str]) -> String {
        let o = self.command(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    }

    /// (open, dnd, count) as the daemon reports it.
    fn state(&self) -> (bool, bool, u32) {
        let s = self.cli_ok(&["state"]);
        let get = |k: &str| kv(&s, k).unwrap_or_else(|| panic!("no {k} in {s:?}")).to_string();
        (get("open") == "true", get("dnd") == "true", get("count").parse().unwrap())
    }

    fn notify(&self, summary: &str, body: &str, actions: &[&str], timeout: i32) -> u32 {
        let r = self
            .client
            .call(Message::call(
                NOTIF_NAME,
                NOTIF_PATH,
                NOTIF_IFACE,
                "Notify",
                vec![
                    Value::s("test-app"),
                    Value::U32(0),
                    Value::s("dialog-information"),
                    Value::s(summary),
                    Value::s(body),
                    Value::strings(actions.iter().copied()),
                    Value::dict_sv(std::iter::empty::<(String, Value)>()),
                    Value::I32(timeout),
                ],
            ))
            .expect("Notify");
        r.body[0].as_u32().unwrap()
    }

    fn signal(&self, member: &str) -> Message {
        loop {
            let m = self.signals.recv_timeout(Duration::from_secs(4)).unwrap_or_else(|_| panic!("no {member} signal\n{}", self.daemon_log()));
            if m.member.as_deref() == Some(member) {
                return m;
            }
        }
    }

    /// Poll until `f` gives something (5 s).
    fn until<T>(&mut self, what: &str, mut f: impl FnMut(&mut Rig) -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            if Instant::now() > end {
                panic!("timed out waiting for {what}\nmock:\n{}\ndaemon:\n{}", self.mock.lines().join("\n"), self.daemon_log());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// Every mapped layer surface the compositor currently has.
    fn surfaces(&mut self) -> Vec<Surf> {
        let from = self.mock.pos();
        self.mock.cmd("state");
        self.mock.wait(from, "the state dump", |l| l.starts_with("state_end"));
        let lines = self.mock.lines();
        lines[from..]
            .iter()
            .filter(|l| l.starts_with("surface "))
            .filter_map(|l| {
                let ns = kv(l, "namespace")?.to_string();
                if kv(l, "mapped") != Some("true") {
                    return None;
                }
                let r = nums(kv(l, "rect")?)?;
                Some(Surf {
                    ns,
                    layer: kv(l, "layer")?.parse().ok()?,
                    anchor: kv(l, "anchor")?.parse().ok()?,
                    zone: kv(l, "zone")?.parse().ok()?,
                    rect: [r[0], r[1], r[2], r[3]],
                    input: kv(l, "input").and_then(nums).map(|v| [v[0], v[1], v[2], v[3]]),
                })
            })
            .collect()
    }

    fn surf(&mut self, ns: &str) -> Option<Surf> {
        self.surfaces().into_iter().find(|s| s.ns == ns)
    }

    /// The panel once it shows exactly `h` logical pixels of content.
    fn panel_ready(&mut self, h: u32) -> Surf {
        self.until("the panel to be drawn at its final size", |r| r.surf("notify-rs").filter(|s| s.rect[3] as u32 == h && s.input.is_some()))
    }

    // ---- layout, computed exactly like the daemon

    fn panel_frame(&mut self, items: &[Notification], hover: Option<HitId>) -> Frame {
        self.panel_frame_with(items, hover, None)
    }

    fn panel_frame_with(&mut self, items: &[Notification], hover: Option<HitId>, media: Option<&MediaView>) -> Frame {
        let refs: Vec<&Notification> = items.iter().collect(); // newest first
        let none = |_: &Notification, _: usize| None;
        let st = PanelState {
            media,
            items: &refs,
            dnd: false,
            now_unix: notify_rs::service::now_unix(),
            hover: hover.as_ref(),
            pressed: None,
            drag: None,
            scroll: 0.0,
            max_card_h: OUT_H - 2.0 * panel::MARGIN - 70.0,
            images: &none,
        };
        panel::draw_panel(&mut Painter::new(None, self.scale, &mut self.fonts), &st)
    }

    fn toast_frame(&mut self, items: &[Notification], hover: Option<HitId>) -> Frame {
        let refs: Vec<&Notification> = items.iter().collect();
        let none = |_: &Notification, _: usize| None;
        panel::draw_toasts(&mut Painter::new(None, self.scale, &mut self.fonts), &refs, notify_rs::service::now_unix(), hover.as_ref(), None, &none)
    }

    fn hit_rect(frame: &Frame, id: &HitId) -> (f32, f32) {
        let h = frame.hits.iter().find(|h| &h.id == id).unwrap_or_else(|| panic!("no {id:?} in {:?}", frame.hits.iter().map(|h| &h.id).collect::<Vec<_>>()));
        (h.r.cx(), h.r.cy())
    }

    fn click_in(&mut self, s: &Surf, at: (f32, f32)) {
        self.mock.cmd(&format!("click {} {}", s.rect[0] as f32 + at.0, s.rect[1] as f32 + at.1));
    }

    fn move_in(&mut self, s: &Surf, at: (f32, f32)) {
        self.mock.cmd(&format!("move {} {}", s.rect[0] as f32 + at.0, s.rect[1] as f32 + at.1));
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("--- daemon stderr ---\n{}", self.daemon_log());
        }
        if let Some(d) = &mut self.daemon {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = self.bus.kill();
        let _ = self.bus.wait();
    }
}

fn note(id: u32, summary: &str, body: &str, actions: &[(&str, &str)]) -> Notification {
    let mut n = sample(id, "test-app", summary, body, 0);
    n.actions = actions.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    n
}

fn pixel(commit: &str, x: usize, y: usize) -> [u8; 4] {
    let path = kv(commit, "png").expect("png dump");
    let img = notify_rs::image::decode(&std::fs::read(path).unwrap()).unwrap();
    let i = (y * img.w + x) * 4;
    [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
}

fn is_commit(l: &str, ns: &str) -> bool {
    l.starts_with("commit") && kv(l, "namespace") == Some(ns)
}

// ---------------------------------------------------------------------------

#[test]
fn a_toast_appears_expires_and_the_notification_stays() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let from = r.mock.pos();
    r.cli_ok(&["send", "-a", "test-app", "-t", "1000", "Hello", "World"]);
    let commit = r.mock.wait(from, "the toast to be drawn", |l| is_commit(l, "notify-rs-toasts"));

    let frame = r.toast_frame(&[note(1, "Hello", "World", &[])], None);
    assert_eq!(kv(&commit, "logical"), Some(format!("{}x{}", frame.w as u32, frame.h.ceil() as u32).as_str()), "the daemon and the layout code agree on the size");
    let card = frame.card.expect("a card");
    let px = pixel(&commit, card.cx() as usize, card.y as usize + 4);
    assert_eq!(px[3], 255, "the card is opaque");
    assert!(px[0] < 70 && px[1] < 70 && px[2] < 70, "dark card, got {px:?}");
    let corner = pixel(&commit, 0, 0);
    assert!(corner[3] < 60, "outside the card it is transparent: {corner:?}");

    let s = r.surf("notify-rs-toasts").expect("toast surface");
    assert_eq!((s.layer, s.anchor, s.zone), (3, 1, 0), "overlay layer, anchored to the top, no exclusive zone");
    let input = s.input.expect("an input region");
    assert!(input[2] < s.rect[2], "only the card takes pointer input, not the transparent margin");
    assert!(r.surf("notify-rs").is_none(), "no panel for a toast");

    r.until("the toast to go away", |r| r.surf("notify-rs-toasts").is_none().then_some(()));
    assert_eq!(r.state(), (false, false, 1), "it stays in the center after the banner is gone");
    r.mock.assert_clean();
}

#[test]
fn the_panel_opens_with_a_scrim_and_a_click_away_closes_it() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let id = r.notify("Mail", "You have one new message", &[], 0);
    r.until("the toast", |r| r.surf("notify-rs-toasts").map(|_| ()));

    r.cli_ok(&["center", "open"]);
    let frame = r.panel_frame(&[note(id, "Mail", "You have one new message", &[])], None);
    let panel = r.panel_ready(frame.h.ceil() as u32);
    assert_eq!(panel.rect[2], frame.w as i32);
    assert_eq!((panel.layer, panel.anchor, panel.zone), (3, 1, 0));
    assert_eq!(panel.rect[0], (1920 - panel.rect[2]) / 2, "centered");
    assert_eq!(panel.rect[1], 0, "right under the bar edge, no margin");

    let scrim = r.surf("notify-rs-scrim").expect("a scrim");
    assert_eq!((scrim.layer, scrim.anchor, scrim.zone), (2, 15, -1), "below the panel, covering everything, never pushing windows");
    assert_eq!(scrim.rect, [0, 0, 1920, 1080]);
    assert!(r.surf("notify-rs-toasts").is_none(), "the toast gives way to the panel");
    assert_eq!(r.state(), (true, false, 1));

    let lines = r.mock.lines().join("\n");
    assert!(lines.contains("keyboard namespace=notify-rs mode=0"), "never steal the keyboard: {lines}");
    assert!(lines.contains("keyboard namespace=notify-rs-scrim mode=0"), "{lines}");

    // the card is drawn
    let commit = r.mock.lines().into_iter().rev().find(|l| is_commit(l, "notify-rs")).unwrap();
    let card = frame.card.unwrap();
    let px = pixel(&commit, card.cx() as usize, card.y as usize + 4);
    assert_eq!(px[3], 255);
    assert!(px[0] < 70 && px[1] < 70 && px[2] < 70, "{px:?}");

    // a click on empty desktop closes the panel, and only the panel
    r.mock.cmd("click 300 600");
    r.until("the panel to close", |r| (!r.state().0).then_some(()));
    r.until("the surfaces to go", |r| (r.surf("notify-rs").is_none() && r.surf("notify-rs-scrim").is_none()).then_some(()));
    assert_eq!(r.state().2, 1, "closing the panel keeps the notifications");
    r.mock.assert_clean();
}

#[test]
fn clicking_inside_the_card_does_not_close_it_and_toggle_works() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let id = r.notify("Plain", "nothing to click", &[], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(id, "Plain", "nothing to click", &[])];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);

    // the card's empty header area: not a button, must not close
    let card = frame.card.unwrap();
    r.click_in(&panel, (card.x + 6.0, card.y + 6.0));
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.state().0, "still open");
    assert!(r.surf("notify-rs").is_some());

    r.cli_ok(&["center", "toggle"]);
    assert!(!r.state().0);
    r.until("closed", |r| r.surf("notify-rs").is_none().then_some(()));
    r.cli_ok(&["center", "toggle"]);
    assert!(r.state().0);
    r.panel_ready(frame.h.ceil() as u32);
    r.mock.assert_clean();
}

#[test]
fn clicking_an_action_button_sends_the_action_and_closes_the_notification() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let acts = [("default", "Open"), ("reply", "Reply"), ("mark", "Mark as read")];
    let id = r.notify("Mika", "dinner?", &["default", "Open", "reply", "Reply", "mark", "Mark as read"], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(id, "Mika", "dinner?", &acts)];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);

    let at = Rig::hit_rect(&frame, &HitId::Action(id, "reply".into()));
    // hovering shows the pointer cursor
    let from = r.mock.pos();
    r.move_in(&panel, at);
    r.mock.wait(from, "a pointer cursor", |l| l.starts_with("cursor") && kv(l, "shape") == Some("4") && kv(l, "valid") == Some("true"));

    r.click_in(&panel, at);
    let m = r.signal("ActionInvoked");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_str()), (Some(id), Some("reply")));
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(id), Some(2)));
    assert_eq!(r.state().2, 0);
    r.mock.assert_clean();
}

#[test]
fn clicking_a_notification_runs_its_default_action() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let id = r.notify("Calendar", "Standup in 5 minutes", &["default", "Open"], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(id, "Calendar", "Standup in 5 minutes", &[("default", "Open")])];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);
    let at = Rig::hit_rect(&frame, &HitId::Item(id));
    r.click_in(&panel, (at.0 - 40.0, at.1)); // away from any button
    let m = r.signal("ActionInvoked");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_str()), (Some(id), Some("default")));
    r.mock.assert_clean();
}

#[test]
fn the_close_button_dismisses_one_and_clear_dismisses_the_rest() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let a = r.notify("first", "one", &[], 0);
    let b = r.notify("second", "two", &[], 0);
    let c = r.notify("third", "three", &[], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(c, "third", "three", &[]), note(b, "second", "two", &[]), note(a, "first", "one", &[])];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);

    // the X only exists while hovering: move there first, then press
    let over = Rig::hit_rect(&frame, &HitId::Item(b));
    r.move_in(&panel, (over.0 - 40.0, over.1));
    let hovered = r.panel_frame(&items, Some(HitId::Item(b)));
    let x = Rig::hit_rect(&hovered, &HitId::Close(b));
    // wait for the daemon to have drawn the hover state before clicking
    let from = r.mock.pos();
    r.mock.wait(from, "a redraw for the hover", |l| is_commit(l, "notify-rs"));
    r.click_in(&panel, x);
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(b), Some(2)), "dismissed by the user");
    assert_eq!(r.state().2, 2);

    // "Clear" in the header
    let items = [note(c, "third", "three", &[]), note(a, "first", "one", &[])];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);
    let clear = Rig::hit_rect(&frame, &HitId::Clear);
    r.click_in(&panel, clear);
    r.until("everything cleared", |r| (r.state().2 == 0).then_some(()));
    r.mock.assert_clean();
}

#[test]
fn the_dnd_toggle_works_and_silences_toasts() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let id = r.notify("seed", "so the list is not empty", &[], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(id, "seed", "so the list is not empty", &[])];
    let frame = r.panel_frame(&items, None);
    let panel = r.panel_ready(frame.h.ceil() as u32);
    let dnd = Rig::hit_rect(&frame, &HitId::Dnd);
    r.click_in(&panel, dnd);
    r.until("do not disturb", |r| r.state().1.then_some(()));
    r.cli_ok(&["center", "close"]);
    r.until("closed", |r| r.surf("notify-rs").is_none().then_some(()));

    r.notify("quiet", "no banner please", &[], 5000);
    std::thread::sleep(Duration::from_millis(500));
    assert!(r.surf("notify-rs-toasts").is_none(), "no toast while DND is on");
    assert_eq!(r.state(), (false, true, 2), "but it is kept");
    r.mock.assert_clean();
}

#[test]
fn clicking_a_toast_hides_it_without_dismissing_the_notification() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    let id = r.notify("Build", "finished", &[], 0); // never expires by itself
    let toast = r.until("the toast", |r| r.surf("notify-rs-toasts"));
    let frame = r.toast_frame(&[note(id, "Build", "finished", &[])], None);
    let at = Rig::hit_rect(&frame, &HitId::Item(id));
    let _ = toast;
    let toast = r.until("the toast, final size", |r| r.surf("notify-rs-toasts").filter(|s| s.rect[3] as u32 == frame.h.ceil() as u32));
    r.click_in(&toast, at);
    r.until("the toast to hide", |r| r.surf("notify-rs-toasts").is_none().then_some(()));
    assert_eq!(r.state(), (false, false, 1));
    r.mock.assert_clean();
}

#[test]
fn the_compositor_closing_the_panel_is_handled() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    r.notify("x", "y", &[], 0);
    r.cli_ok(&["center", "open"]);
    r.until("the panel", |r| r.surf("notify-rs"));
    r.mock.cmd("close notify-rs");
    r.until("the daemon to notice", |r| (!r.state().0).then_some(()));
    r.until("scrim gone too", |r| r.surf("notify-rs-scrim").is_none().then_some(()));
    // and it can be opened again
    r.cli_ok(&["center", "open"]);
    r.until("the panel again", |r| r.surf("notify-rs"));
    r.mock.assert_clean();
}

#[test]
fn losing_the_compositor_ends_the_daemon_with_a_message() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.run_daemon();
    r.mock.cmd("disconnect");
    let mut d = r.daemon.take().unwrap();
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = d.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "the daemon is still running after the compositor went away");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    assert!(r.daemon_log().contains("Wayland"), "{}", r.daemon_log());
}

#[test]
fn a_compositor_without_layer_shell_is_refused_before_taking_the_name() {
    let Some(mut r) = Rig::start(&["--without", "layer_shell"]) else { return };
    let mut d = r.spawn_daemon();
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = d.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "daemon should have exited");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let log = r.daemon_log();
    assert!(log.contains("wlr-layer-shell"), "{log}");
    assert!(!r.command(&["state"]).output().unwrap().status.success(), "the notification name must stay free for another daemon");
}

// ---------------------------------------------------------------- media card

const PLAYER: &str = "org.mpris.MediaPlayer2.firefox.instance_1_100";

fn track_view() -> MediaView {
    MediaView {
        identity: "Mock Browser".into(),
        playing: true,
        title: "Song".into(),
        artist: "Band".into(),
        album: "Album".into(),
        position_us: 12_000_000,
        length_us: 200_000_000,
        volume: 0.5,
        can_seek: true,
        art: None,
    }
}

/// A rig with a playing fake browser and the panel open at its final size.
fn playing_rig(art: Option<String>) -> Option<(Rig, Player, Surf, Frame)> {
    let mut r = Rig::start(&[])?;
    let player = Player::start(&r.address, PLAYER, State { status: "Playing".into(), art: art.unwrap_or_default(), ..State::default() });
    r.run_daemon();
    r.cli_ok(&["center", "open"]);
    let frame = r.panel_frame_with(&[], None, Some(&track_view()));
    let panel = r.panel_ready(frame.h.ceil() as u32);
    Some((r, player, panel, frame))
}

fn last_call(p: &Player, prefix: &str) -> Option<String> {
    p.calls().into_iter().rev().find(|c| c.starts_with(prefix))
}

fn wait_call(r: &mut Rig, p: &Player, what: &str, f: impl Fn(&[String]) -> bool) {
    r.until(what, |_| f(&p.calls()).then_some(()));
}

#[test]
fn the_media_buttons_control_the_player() {
    let Some((mut r, p, panel, frame)) = playing_rig(None) else { return };
    r.click_in(&panel, Rig::hit_rect(&frame, &HitId::PlayPause));
    wait_call(&mut r, &p, "PlayPause", |c| c.iter().any(|x| x == "PlayPause"));
    r.click_in(&panel, Rig::hit_rect(&frame, &HitId::Next));
    wait_call(&mut r, &p, "Next", |c| c.iter().any(|x| x == "Next"));
    r.click_in(&panel, Rig::hit_rect(&frame, &HitId::Prev));
    wait_call(&mut r, &p, "Previous", |c| c.iter().any(|x| x == "Previous"));
    assert_eq!(p.calls(), ["PlayPause", "Next", "Previous"], "one command per click, in order");
    r.mock.assert_clean();
}

#[test]
fn the_volume_slider_follows_clicks_drags_the_wheel_and_mute() {
    let Some((mut r, p, panel, frame)) = playing_rig(None) else { return };
    let vol = frame.volume.expect("a volume slider");
    let at = |f: f32| (vol.x + vol.w * f, vol.cy());

    // click at 80 %
    r.click_in(&panel, at(0.8));
    wait_call(&mut r, &p, "volume 0.80", |c| c.iter().any(|x| x == "SetVolume 0.80"));

    // drag down to 30 %: the last value the player gets is where the knob ended
    r.move_in(&panel, at(0.7));
    r.mock.cmd("press");
    for f in [0.6, 0.5, 0.4, 0.3] {
        r.move_in(&panel, at(f));
    }
    r.mock.cmd("release_button");
    r.until("the drag to end at 0.30", |_| (last_call(&p, "SetVolume").as_deref() == Some("SetVolume 0.30")).then_some(()));

    // the wheel over the slider: 5 % per notch (down = away from you)
    r.move_in(&panel, at(0.3));
    r.mock.cmd("axis 15");
    r.until("0.25", |_| (last_call(&p, "SetVolume").as_deref() == Some("SetVolume 0.25")).then_some(()));
    r.mock.cmd("axis -15");
    r.until("0.30 again", |_| (last_call(&p, "SetVolume").as_deref() == Some("SetVolume 0.30")).then_some(()));

    // mute remembers where it was
    r.click_in(&panel, Rig::hit_rect(&frame, &HitId::Mute));
    r.until("mute", |_| (last_call(&p, "SetVolume").as_deref() == Some("SetVolume 0.00")).then_some(()));
    r.click_in(&panel, Rig::hit_rect(&frame, &HitId::Mute));
    r.until("unmute", |_| (last_call(&p, "SetVolume").as_deref() == Some("SetVolume 0.30")).then_some(()));
    r.mock.assert_clean();
}

#[test]
fn seeking_commits_where_the_knob_is_released() {
    let Some((mut r, p, panel, frame)) = playing_rig(None) else { return };
    let seek = frame.seek.expect("a seek bar");
    let at = |f: f32| (seek.x + seek.w * f, seek.cy());

    // a plain click at 25 % of 200 s
    r.click_in(&panel, at(0.25));
    r.until("SetPosition near 50 s", |_| {
        last_call(&p, "SetPosition").and_then(|c| c.rsplit(' ').next().and_then(|n| n.parse::<i64>().ok())).filter(|us| (us - 50_000_000).abs() < 1_000_000).map(|_| ())
    });
    assert!(last_call(&p, "SetPosition").unwrap().contains("/org/mock/track/1"), "addressed to the track: {:?}", p.calls());

    // dragging does not seek until release
    let before = p.calls().len();
    r.move_in(&panel, at(0.5));
    r.mock.cmd("press");
    r.move_in(&panel, at(0.6));
    r.move_in(&panel, at(0.75));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(p.calls().len(), before, "no seeking while the knob is held: {:?}", p.calls());
    r.mock.cmd("release_button");
    r.until("SetPosition near 150 s", |_| {
        last_call(&p, "SetPosition").and_then(|c| c.rsplit(' ').next().and_then(|n| n.parse::<i64>().ok())).filter(|us| (us - 150_000_000).abs() < 1_000_000).map(|_| ())
    });
    assert_eq!(p.calls().len(), before + 1, "exactly one seek for the whole drag");
    r.mock.assert_clean();
}

#[test]
fn cover_art_from_a_file_url_is_drawn() {
    let Some(mut r) = Rig::start(&[]) else { return };
    let png = notify_rs::gfx::png_write::encode_rgba(300, 300, &[200, 30, 30, 255].repeat(300 * 300));
    let path = r.mock.dir.join("cover.png");
    std::fs::write(&path, png).unwrap();
    let _p = Player::start(&r.address, PLAYER, State { status: "Paused".into(), art: format!("file://{}", path.display()), ..State::default() });
    r.run_daemon();
    r.cli_ok(&["center", "open"]);
    let red = |commit: &str| {
        let img = notify_rs::image::decode(&std::fs::read(kv(commit, "png").unwrap()).unwrap()).unwrap();
        img.rgba.chunks(4).filter(|p| (p[0] as i32 - 200).abs() < 8 && (p[1] as i32 - 30).abs() < 8 && (p[2] as i32 - 30).abs() < 8 && p[3] == 255).count()
    };
    r.until("the cover to appear", |r| {
        let c = r.mock.lines().into_iter().rev().find(|l| is_commit(l, "notify-rs"))?;
        (red(&c) > 4500).then_some(())
    });
    r.mock.assert_clean();
}

#[test]
fn the_clock_redraws_once_a_second_while_playing_and_not_at_all_when_paused() {
    let Some((mut r, p, _panel, _frame)) = playing_rig(None) else { return };
    let commits = |r: &mut Rig| r.mock.lines().iter().filter(|l| is_commit(l, "notify-rs")).count();
    std::thread::sleep(Duration::from_millis(300));
    let a = commits(&mut r);
    std::thread::sleep(Duration::from_millis(2300));
    let b = commits(&mut r);
    assert!((2..=4).contains(&(b - a)), "about one redraw per second while playing, got {}", b - a);

    p.announce(|s| s.status = "Paused".into());
    std::thread::sleep(Duration::from_millis(600)); // the pause itself redraws the button
    let a = commits(&mut r);
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(commits(&mut r), a, "a paused player must not make the panel redraw");
    r.mock.assert_clean();
}

#[test]
fn fractional_scales_get_matching_buffers_and_clicks_still_land() {
    let Some(mut r) = Rig::start(&[]) else { return };
    r.mock.cmd("scale 180"); // 1.5x
    r.scale = 1.5;
    r.run_daemon();
    let id = r.notify("Mika", "dinner?", &["default", "Open", "reply", "Reply"], 0);
    r.cli_ok(&["center", "open"]);
    let items = [note(id, "Mika", "dinner?", &[("default", "Open"), ("reply", "Reply")])];
    let frame = r.panel_frame(&items, None);
    let (w, h) = (frame.w as i32, frame.h.ceil() as i32);
    r.panel_ready(h as u32);
    let want = format!("{}x{}", (w * 180 + 60) / 120, (h * 180 + 60) / 120);
    r.until("a 1.5x buffer", |r| r.mock.lines().iter().rev().find(|l| is_commit(l, "notify-rs")).filter(|l| kv(l, "buffer") == Some(want.as_str()) && kv(l, "viewport") == Some(format!("{w}x{h}").as_str())).map(|_| ()));

    // the compositor moves the panel to a 2x output
    r.mock.cmd("scale 240");
    r.scale = 2.0;
    let frame2 = r.panel_frame(&items, None);
    let (w2, h2) = (frame2.w as i32, frame2.h.ceil() as i32);
    let want = format!("{}x{}", w2 * 2, h2 * 2);
    r.until("a 2x buffer", |r| r.mock.lines().iter().rev().find(|l| is_commit(l, "notify-rs")).filter(|l| kv(l, "buffer") == Some(want.as_str())).map(|_| ()));
    let panel = r.until("the panel at its 2x size", |r| r.surf("notify-rs").filter(|s| s.rect[3] == h2));

    let at = Rig::hit_rect(&frame2, &HitId::Action(id, "reply".into()));
    r.click_in(&panel, at);
    let m = r.signal("ActionInvoked");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_str()), (Some(id), Some("reply")));
    r.mock.assert_clean();
}
