//! The `notify-rs` binary end to end: a real dbus-daemon, the real headless
//! daemon process, and the command line talking to it.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use notify_rs::dbus::{Conn, Error, Message, Value};
use notify_rs::service::{CTL_IFACE, CTL_NAME, CTL_PATH};

struct Bus {
    child: Child,
    address: String,
}

impl Bus {
    fn start() -> Option<Bus> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        let address = line.trim().to_owned();
        (!address.is_empty()).then_some(Bus { child, address })
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_notify-rs"))
            .args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.cmd(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    }

    fn daemon(&self) -> Child {
        self.daemon_with(&[], &[])
    }

    /// A headless daemon with extra arguments and environment variables.
    fn daemon_with(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        let child = Command::new(env!("CARGO_BIN_EXE_notify-rs"))
            .args(["daemon", "--headless"])
            .args(args)
            .envs(env.iter().copied())
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            if self.cmd(&["state"]).status.success() {
                return child;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not come up");
    }
}

impl Bus {
    fn conn(&self) -> Conn {
        Conn::connect(&self.address).unwrap().0
    }

    /// The daemon's `Theme` property.
    fn theme(&self) -> String {
        let r = self
            .conn()
            .call(Message::call(CTL_NAME, CTL_PATH, "org.freedesktop.DBus.Properties", "Get", vec![Value::s(CTL_IFACE), Value::s("Theme")]))
            .unwrap();
        r.body[0].as_str().unwrap().to_string()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn send_center_dnd_clear_and_state() {
    let Some(bus) = Bus::start() else { return eprintln!("skip: no dbus-daemon") };
    let _d = Kill(bus.daemon());

    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=0");
    let id = bus.ok(&["send", "-p", "Hello 日本語", "A body"]);
    assert_eq!(id, "1");
    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=1");

    let again = bus.ok(&["send", "-p", "-r", &id, "Hello again"]);
    assert_eq!(again, id, "replaced in place");
    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=1");

    bus.ok(&["send", "-u", "critical", "-t", "0", "-h", "int:value:40", "Alarm"]);
    bus.ok(&["send", "-u", "low", "-h", "string:x-canonical-private-synchronous:vol", "-h", "int:value:10", "Volume"]);
    bus.ok(&["send", "-h", "string:x-canonical-private-synchronous:vol", "-h", "int:value:20", "Volume"]);
    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=3", "sync tag replaced in place");

    bus.ok(&["center", "open"]);
    assert!(bus.ok(&["state"]).starts_with("open=true"));
    bus.ok(&["center"]);
    assert!(bus.ok(&["state"]).starts_with("open=false"));
    bus.ok(&["center", "toggle"]);
    bus.ok(&["center", "close"]);
    assert!(bus.ok(&["state"]).starts_with("open=false"));

    bus.ok(&["dnd", "on"]);
    assert!(bus.ok(&["state"]).contains("dnd=true"));
    bus.ok(&["dnd"]);
    assert!(bus.ok(&["state"]).contains("dnd=false"));
    bus.ok(&["dnd", "toggle"]);
    bus.ok(&["dnd", "off"]);

    bus.ok(&["clear"]);
    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=0");

    // argument errors are exit code 2 with a message
    let o = bus.cmd(&["send"]);
    assert_eq!(o.status.code(), Some(2));
    let o = bus.cmd(&["center", "sideways"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn second_daemon_is_refused() {
    let Some(bus) = Bus::start() else { return eprintln!("skip") };
    let _d = Kill(bus.daemon());
    let o = bus.cmd(&["daemon", "--headless"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("already running"), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn commands_explain_when_no_daemon_is_running() {
    let Some(bus) = Bus::start() else { return eprintln!("skip") };
    for args in [&["center"][..], &["dnd", "on"], &["clear"], &["state"]] {
        let o = bus.cmd(args);
        assert!(!o.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("not running"), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    let o = bus.cmd(&["send", "x"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("no notification daemon"));
}

#[test]
fn bell_feed_tracks_count_dnd_and_daemon_lifetime() {
    let Some(bus) = Bus::start() else { return eprintln!("skip") };

    let mut bell = Command::new(env!("CARGO_BIN_EXE_notify-rs"))
        .args(["bell"])
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = bell.id();
    let mut lines = BufReader::new(bell.stdout.take().unwrap()).lines();
    let _kill = Kill(bell);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let mut next = || lines.next().expect("bell feed ended").unwrap();

    // daemon not running yet
    assert!(next().contains(r#""class":"offline""#));

    let d = Kill(bus.daemon());
    let l = next();
    assert!(l.contains(r#""class":"empty""#), "{l}");
    // the text waybar shows is exactly "icon<space>": no clipping at the module edge
    assert!(l.starts_with("{\"text\":\"\u{f009a} \","), "{l}");

    bus.ok(&["send", "one"]);
    let l = next();
    assert!(l.contains(r#""class":"has""#) && l.contains("1 notification"), "{l}");
    assert!(l.starts_with("{\"text\":\"\u{f116b} \","), "{l}");
    bus.ok(&["send", "two"]);
    let l = next();
    assert!(l.contains("2 notifications") && l.contains(r#""percentage":2"#), "{l}");

    bus.ok(&["dnd", "on"]);
    let l = next();
    assert!(l.contains(r#""class":"dnd""#), "{l}");
    assert!(l.starts_with("{\"text\":\"\u{f009b} \","), "{l}");

    bus.ok(&["center", "open"]);
    let l = next();
    assert!(l.contains("dnd open"), "{l}");

    drop(d); // daemon dies: the bell must say so
    let l = next();
    assert!(l.contains(r#""class":"offline""#), "{l}");
}

#[test]
fn the_theme_command_switches_and_rejects_nonsense() {
    let Some(bus) = Bus::start() else { return eprintln!("skip: no dbus-daemon") };
    let _d = Kill(bus.daemon());
    assert_eq!(bus.theme(), "default", "the look you had before is the default");

    bus.ok(&["theme", "retro"]);
    assert_eq!(bus.theme(), "retro");
    bus.ok(&["theme", "retro"]);
    assert_eq!(bus.theme(), "retro", "setting is not toggling");
    bus.ok(&["theme"]);
    assert_eq!(bus.theme(), "default", "no argument toggles");
    bus.ok(&["theme", "toggle"]);
    assert_eq!(bus.theme(), "retro");
    bus.ok(&["theme", "default"]);
    assert_eq!(bus.theme(), "default");
    bus.ok(&["theme", "WindowMaker"]);
    assert_eq!(bus.theme(), "retro", "the Window Maker name works too");

    // a typo is refused by the command itself, and by the daemon if it is asked directly
    let o = bus.cmd(&["theme", "bogus"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("expected toggle, default or retro"), "{}", String::from_utf8_lossy(&o.stderr));
    let e = bus.conn().call(Message::call(CTL_NAME, CTL_PATH, CTL_IFACE, "SetTheme", vec![Value::s("bogus")])).unwrap_err();
    assert!(matches!(&e, Error::Remote { name, .. } if name == "org.freedesktop.DBus.Error.InvalidArgs"), "{e}");
    assert_eq!(bus.theme(), "retro", "and nothing changed");

    // the rest of the daemon is unaffected
    bus.ok(&["send", "-p", "still works"]);
    assert_eq!(bus.ok(&["state"]), "open=false dnd=false count=1");
}

#[test]
fn the_daemon_starts_in_the_theme_it_is_told() {
    for (args, env, want) in [
        (vec![], vec![], "default"),
        (vec!["--theme", "retro"], vec![], "retro"),
        (vec![], vec![("NOTIFY_RS_THEME", "retro")], "retro"),
        (vec!["--theme", "default"], vec![("NOTIFY_RS_THEME", "retro")], "default"), // the flag wins
        (vec![], vec![("NOTIFY_RS_THEME", "")], "default"),                          // empty means unset
    ] {
        let Some(bus) = Bus::start() else { return eprintln!("skip: no dbus-daemon") };
        let _d = Kill(bus.daemon_with(&args, &env));
        assert_eq!(bus.theme(), want, "args {args:?} env {env:?}");
    }
}

#[test]
fn a_bad_theme_at_startup_is_refused_before_taking_the_bus_name() {
    let Some(bus) = Bus::start() else { return eprintln!("skip: no dbus-daemon") };
    for (args, env) in [(vec!["--theme", "neon"], vec![]), (vec!["--theme"], vec![]), (vec![], vec![("NOTIFY_RS_THEME", "neon")])] {
        let o = Command::new(env!("CARGO_BIN_EXE_notify-rs"))
            .args(["daemon", "--headless"])
            .args(&args)
            .envs(env.iter().copied())
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?} {env:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("default or retro"), "says what is accepted: {err}");
    }
    assert!(!bus.cmd(&["state"]).status.success(), "no daemon was left behind");
}
