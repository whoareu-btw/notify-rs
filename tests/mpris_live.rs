//! MPRIS tracker, commands and the CLI, against mock players on a real
//! `dbus-daemon`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

mod common;

use common::player::{Mock, State};
use notify_rs::dbus::{Conn, Message};
use notify_rs::model::Status;
use notify_rs::mpris::{self, Tracker};

struct Daemon {
    child: Child,
    address: String,
}

impl Daemon {
    fn start() -> Option<Daemon> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        let address = line.trim().to_owned();
        (!address.is_empty()).then_some(Daemon { child, address })
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const FF: &str = "org.mpris.MediaPlayer2.firefox.instance_1_100";
const CH: &str = "org.mpris.MediaPlayer2.chromium.instance200";

fn setup() -> Option<(Daemon, Conn, Receiver<Message>)> {
    let d = Daemon::start()?;
    let (c, rx) = Conn::connect(&d.address).unwrap();
    Some((d, c, rx))
}

#[test]
fn reads_a_player() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip: no dbus-daemon") };
    let _m = Mock::start(&d.address, FF, State { status: "Playing".into(), ..Default::default() });
    let snap = Tracker::new().current(&c).unwrap().unwrap();
    assert_eq!(snap.bus_name, FF);
    assert_eq!(snap.identity, "Mock Browser");
    assert_eq!(snap.status, Status::Playing);
    assert_eq!(snap.track.title, "Song");
    assert_eq!(snap.track.artists, vec!["Band"]);
    assert_eq!(snap.track.length_us, 200_000_000);
    assert_eq!(snap.track.track_id.as_deref(), Some("/org/mock/track/1"));
    assert_eq!(snap.position_us, 12_000_000);
    assert!((snap.volume - 0.5).abs() < 1e-9);
    assert!(snap.can_seek);
}

#[test]
fn no_player_is_none() {
    let Some((_d, c, _rx)) = setup() else { return eprintln!("skip") };
    assert!(Tracker::new().current(&c).unwrap().is_none());
}

#[test]
fn position_falls_back_to_a_separate_get() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip") };
    let _m = Mock::start(&d.address, FF, State { position_in_getall: false, position: 77_000_000, ..Default::default() });
    let snap = Tracker::new().current(&c).unwrap().unwrap();
    assert_eq!(snap.position_us, 77_000_000);
}

#[test]
fn playing_player_wins_and_sticky_survives_pause() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip") };
    let ff = Mock::start(&d.address, FF, State::default()); // paused
    let ch = Mock::start(&d.address, CH, State { status: "Playing".into(), title: "Other".into(), ..Default::default() });
    let mut t = Tracker::new();
    assert_eq!(t.current(&c).unwrap().unwrap().bus_name, CH, "the playing one");
    // chromium pauses: we must stay on it, not jump to firefox
    ch.update(|s| s.status = "Paused".into());
    assert_eq!(t.current(&c).unwrap().unwrap().bus_name, CH, "sticky when nothing plays");
    // firefox starts: now follow it
    ff.update(|s| s.status = "Playing".into());
    assert_eq!(t.current(&c).unwrap().unwrap().bus_name, FF);
    // both playing: keep the one already shown
    ch.update(|s| s.status = "Playing".into());
    assert_eq!(t.current(&c).unwrap().unwrap().bus_name, FF);
}

#[test]
fn empty_metadata_during_a_tab_switch_keeps_the_last_good_track() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State { status: "Playing".into(), ..Default::default() });
    let mut t = Tracker::new();
    assert_eq!(t.current(&c).unwrap().unwrap().track.title, "Song");
    m.update(|s| s.metadata_empty = true);
    let snap = t.current(&c).unwrap().unwrap();
    assert_eq!(snap.track.title, "Song", "kept through the blip");
    assert_eq!(snap.status, Status::Playing);
    // real new metadata replaces it
    m.update(|s| {
        s.metadata_empty = false;
        s.title = "Next".into();
    });
    assert_eq!(t.current(&c).unwrap().unwrap().track.title, "Next");
}

#[test]
fn player_that_goes_away_is_forgotten() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State::default());
    let mut t = Tracker::new();
    assert!(t.current(&c).unwrap().is_some());
    m.conn.close();
    std::thread::sleep(Duration::from_millis(200));
    assert!(t.current(&c).unwrap().is_none());
    assert_eq!(t.sticky(), None);
}

#[test]
fn commands_reach_the_player() {
    let Some((d, c, _rx)) = setup() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State::default());
    mpris::play_pause(&c, FF).unwrap();
    mpris::pause(&c, FF).unwrap();
    mpris::next(&c, FF).unwrap();
    mpris::previous(&c, FF).unwrap();
    mpris::set_volume(&c, FF, 0.8).unwrap();
    assert!((mpris::volume_step(&c, FF, 0.05).unwrap() - 0.85).abs() < 1e-9);
    assert!((mpris::volume_step(&c, FF, 0.5).unwrap() - 1.0).abs() < 1e-9, "clamped at 100%");
    mpris::seek_to(&c, FF, Some("/org/mock/track/1"), 90_000_000, 12_000_000).unwrap();
    mpris::seek_to(&c, FF, None, 100_000_000, 90_000_000).unwrap();
    assert_eq!(
        m.calls(),
        vec![
            "PlayPause",
            "Pause",
            "Next",
            "Previous",
            "SetVolume 0.80",
            "SetVolume 0.85",
            "SetVolume 1.00",
            "SetPosition /org/mock/track/1 90000000",
            "Seek 10000000",
        ]
    );
}

#[test]
fn wait_for_change_wakes_on_signals_and_coalesces_bursts() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State::default());
    let mut client = mpris::Client::new_at(&d.address).unwrap();
    client.subscribe().unwrap();
    assert!(!client.wait_for_change(Duration::from_millis(150)).unwrap(), "quiet bus: timeout");

    // a burst like a track change
    for i in 0..5 {
        m.announce(|s| s.title = format!("T{i}"));
    }
    assert!(client.wait_for_change(Duration::from_secs(2)).unwrap());
    assert!(!client.wait_for_change(Duration::from_millis(150)).unwrap(), "the burst was merged into one wake-up");
    assert_eq!(client.current().unwrap().unwrap().track.title, "T4");

    // a player appearing also counts
    let _ch = Mock::start(&d.address, CH, State::default());
    assert!(client.wait_for_change(Duration::from_secs(2)).unwrap());
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_notify-rs"))
}

#[test]
fn cli_status_toggle_vol() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State { status: "Playing".into(), ..Default::default() });

    let out = bin().arg("status").env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[playing] Band - Song (0:12 / 3:20) vol 50%");

    let out = bin().arg("toggle").env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(m.state.lock().unwrap().status, "Paused");

    let out = bin().args(["vol", "+10"]).env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "60");
    let out = bin().args(["vol", "25"]).env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "25");

    let out = bin().args(["vol", "loud"]).env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = bin().arg("art").env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Mock Browser") && text.contains("file:///tmp/x.png"), "{text}");
}

#[test]
fn cli_without_players_or_bus_fails_cleanly() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let out = bin().arg("status").env("DBUS_SESSION_BUS_ADDRESS", &d.address).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no player running"));

    let out = bin()
        .arg("status")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus")
        .env("XDG_RUNTIME_DIR", "/nonexistent")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("session bus"));
}

#[test]
fn waybar_feed_follows_the_player_and_survives_tab_switches() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let m = Mock::start(&d.address, FF, State { status: "Playing".into(), ..Default::default() });
    let mut child = bin()
        .args(["waybar", "--icon-only"])
        .env("DBUS_SESSION_BUS_ADDRESS", &d.address)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // watchdog: if the feed stalls, killing it closes the pipe and the test fails instead of hanging
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let mut next = || lines.next().expect("feed ended").unwrap();

    let l = next();
    assert!(l.contains(r#""class":"playing""#) && l.starts_with("{\"text\":\"\u{f04b}\""), "{l}");

    m.announce(|s| s.status = "Paused".into());
    let l = next();
    assert!(l.contains(r#""class":"paused""#) && l.starts_with("{\"text\":\"\u{f03e4}\""), "{l}");
    assert!(l.contains("Band") && l.contains("Song"), "tooltip keeps the info: {l}");

    // tab switch: metadata blanks out while paused; the module must not vanish
    m.announce(|s| s.metadata_empty = true);
    m.announce(|s| {
        s.metadata_empty = false;
        s.title = "Song2".into();
        s.status = "Playing".into();
    });
    let l = next();
    assert!(l.contains("Song2") && l.contains(r#""class":"playing""#), "{l}");

    // the browser closing empties the module (waybar hides it)
    m.conn.close();
    let l = next();
    assert!(l.contains(r#""class":"none""#), "{l}");
    let _ = child.kill();
    let _ = child.wait();
}
