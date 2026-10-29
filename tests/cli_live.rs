//! The `notify-rs` binary end to end: a real dbus-daemon, the real headless
//! daemon process, and the command line talking to it.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
        let child = Command::new(env!("CARGO_BIN_EXE_notify-rs"))
            .args(["daemon", "--headless"])
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
