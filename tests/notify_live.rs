//! The notification service on a real `dbus-daemon`: what apps (and libnotify's
//! GDBus calls) see.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use notify_rs::dbus::{Conn, Error, Message, Value};
use notify_rs::service::{self, Service, CTL_IFACE, CTL_NAME, CTL_PATH, NOTIF_IFACE, NOTIF_NAME, NOTIF_PATH};

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

/// A bus with the headless notification service running, plus a client and a
/// signal listener.
struct Rig {
    d: Daemon,
    client: Conn,
    _client_rx: Receiver<Message>,
    signals: Receiver<Message>,
    _listener: Conn,
}

fn rig() -> Option<Rig> {
    let d = Daemon::start()?;
    let (conn, rx) = Conn::connect(&d.address).unwrap();
    service::claim_names(&conn).expect("claim names");
    let svc = Service::new(conn);
    std::thread::spawn(move || service::run_headless(svc, rx));

    let (listener, signals) = Conn::connect(&d.address).unwrap();
    listener.add_match("type='signal',interface='org.freedesktop.Notifications'").unwrap();
    listener.add_match("type='signal',interface='dev.notifyrs.Daemon'").unwrap();
    let (client, client_rx) = Conn::connect(&d.address).unwrap();
    Some(Rig { d, client, _client_rx: client_rx, signals, _listener: listener })
}

impl Rig {
    fn call(&self, path: &str, iface: &str, member: &str, body: Vec<Value>) -> Result<Message, Error> {
        self.client.call(Message::call(NOTIF_NAME, path, iface, member, body))
    }

    fn notify(&self, replaces: u32, summary: &str, actions: &[&str], hints: Vec<(&str, Value)>, timeout: i32) -> u32 {
        let r = self
            .call(
                NOTIF_PATH,
                NOTIF_IFACE,
                "Notify",
                vec![
                    Value::s("test-app"),
                    Value::U32(replaces),
                    Value::s("dialog-information"),
                    Value::s(summary),
                    Value::s("body <b>text</b>"),
                    Value::strings(actions.iter().copied()),
                    Value::dict_sv(hints.into_iter().map(|(k, v)| (k.to_string(), v))),
                    Value::I32(timeout),
                ],
            )
            .expect("Notify");
        r.body[0].as_u32().unwrap()
    }

    fn ctl(&self, member: &str, body: Vec<Value>) -> Message {
        self.call(CTL_PATH, CTL_IFACE, member, body).expect(member)
    }

    fn state(&self) -> (bool, bool, u32) {
        let r = self.ctl("GetState", vec![]);
        (r.body[0].as_bool().unwrap(), r.body[1].as_bool().unwrap(), r.body[2].as_u32().unwrap())
    }

    /// Next signal named `member` (others are skipped).
    fn signal(&self, member: &str) -> Message {
        loop {
            let m = self.signals.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| panic!("no {member} signal"));
            if m.member.as_deref() == Some(member) && m.path.as_deref().is_some_and(|p| p.starts_with("/org/freedesktop/Notifications") || p.starts_with("/dev/notifyrs")) {
                return m;
            }
        }
    }
}

#[test]
fn server_information_and_capabilities() {
    let Some(r) = rig() else { return eprintln!("skip: no dbus-daemon") };
    let info = r.call(NOTIF_PATH, NOTIF_IFACE, "GetServerInformation", vec![]).unwrap();
    assert_eq!(info.body[0].as_str(), Some("notify-rs"));
    assert_eq!(info.body[3].as_str(), Some("1.2"));
    let caps = r.call(NOTIF_PATH, NOTIF_IFACE, "GetCapabilities", vec![]).unwrap();
    let caps = caps.body[0].as_strings();
    for c in ["actions", "body", "body-markup", "icon-static"] {
        assert!(caps.iter().any(|x| x == c), "missing {c}: {caps:?}");
    }
}

#[test]
fn notify_replace_close_and_signals() {
    let Some(r) = rig() else { return eprintln!("skip") };
    let a = r.notify(0, "one", &[], vec![], -1);
    let b = r.notify(0, "two", &[], vec![], -1);
    assert_ne!(a, b);
    assert_eq!(r.state().2, 2);

    let a2 = r.notify(a, "one, edited", &[], vec![], -1);
    assert_eq!(a2, a, "same id back when replacing");
    assert_eq!(r.state().2, 2);

    r.call(NOTIF_PATH, NOTIF_IFACE, "CloseNotification", vec![Value::U32(a)]).unwrap();
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(a), Some(3)), "closed by the app");
    assert_eq!(r.state().2, 1);

    // closing something that is not there is harmless
    r.call(NOTIF_PATH, NOTIF_IFACE, "CloseNotification", vec![Value::U32(9999)]).unwrap();

    r.ctl("Dismiss", vec![Value::U32(b)]);
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(b), Some(2)), "dismissed by the user");
    assert_eq!(r.state().2, 0);
}

#[test]
fn actions_are_reported_then_the_notification_closes() {
    let Some(r) = rig() else { return eprintln!("skip") };
    let id = r.notify(0, "msg", &["default", "Open", "reply", "Reply"], vec![], -1);
    r.ctl("Invoke", vec![Value::U32(id), Value::s("reply")]);
    let m = r.signal("ActionInvoked");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_str()), (Some(id), Some("reply")));
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(id), Some(2)));

    // resident notifications stay
    let id = r.notify(0, "res", &["default", "Open"], vec![("resident", Value::Bool(true))], -1);
    r.ctl("Invoke", vec![Value::U32(id), Value::s("default")]);
    let _ = r.signal("ActionInvoked");
    assert_eq!(r.state().2, 1);
}

#[test]
fn do_not_disturb_and_state_signal() {
    let Some(r) = rig() else { return eprintln!("skip") };
    assert_eq!(r.state(), (false, false, 0));
    r.ctl("ToggleDnd", vec![]);
    let m = r.signal("StateChanged");
    assert_eq!((m.body[0].as_bool(), m.body[1].as_bool(), m.body[2].as_u32()), (Some(false), Some(true), Some(0)));
    r.notify(0, "x", &[], vec![], -1);
    let m = r.signal("StateChanged");
    assert_eq!(m.body[2].as_u32(), Some(1), "count follows");
    r.ctl("SetDnd", vec![Value::Bool(false)]);
    assert_eq!(r.state(), (false, false, 1));
    r.ctl("ClearAll", vec![]);
    let m = r.signal("NotificationClosed");
    assert_eq!(m.body[1].as_u32(), Some(2));
    assert_eq!(r.state().2, 0);
}

#[test]
fn panel_commands_update_the_open_flag() {
    let Some(r) = rig() else { return eprintln!("skip") };
    r.ctl("Toggle", vec![]);
    assert!(r.state().0);
    let m = r.signal("StateChanged");
    assert_eq!(m.body[0].as_bool(), Some(true));
    r.ctl("Toggle", vec![]);
    assert!(!r.state().0);
    r.ctl("Open", vec![]);
    r.ctl("Open", vec![]);
    assert!(r.state().0);
    r.ctl("Close", vec![]);
    assert!(!r.state().0);
}

#[test]
fn transient_notifications_expire_with_reason_1() {
    let Some(r) = rig() else { return eprintln!("skip") };
    let id = r.notify(0, "osd", &[], vec![("transient", Value::Bool(true))], 1000);
    assert_eq!(r.state().2, 1);
    let m = r.signal("NotificationClosed");
    assert_eq!((m.body[0].as_u32(), m.body[1].as_u32()), (Some(id), Some(1)));
    assert_eq!(r.state().2, 0);
}

#[test]
fn normal_notifications_outlive_their_toast() {
    let Some(r) = rig() else { return eprintln!("skip") };
    r.notify(0, "stays", &[], vec![], 1000);
    std::thread::sleep(Duration::from_millis(1400));
    assert_eq!(r.state().2, 1, "still in the center after the toast timed out");
}

#[test]
fn bad_calls_get_errors_not_silence() {
    let Some(r) = rig() else { return eprintln!("skip") };
    match r.call(NOTIF_PATH, NOTIF_IFACE, "Notify", vec![Value::s("only one arg")]) {
        Err(Error::Remote { name, .. }) => assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs"),
        other => panic!("{other:?}"),
    }
    match r.call(NOTIF_PATH, NOTIF_IFACE, "Nope", vec![]) {
        Err(Error::Remote { name, .. }) => assert_eq!(name, "org.freedesktop.DBus.Error.UnknownMethod"),
        other => panic!("{other:?}"),
    }
    match r.call("/wrong/path", NOTIF_IFACE, "GetCapabilities", vec![]) {
        Err(Error::Remote { name, .. }) => assert_eq!(name, "org.freedesktop.DBus.Error.UnknownObject"),
        other => panic!("{other:?}"),
    }
    // and the service is still alive afterwards
    assert_eq!(r.state().2, 0);
}

#[test]
fn image_data_hint_is_accepted_over_the_wire() {
    let Some(r) = rig() else { return eprintln!("skip") };
    let img = Value::Struct(vec![
        Value::I32(2),
        Value::I32(2),
        Value::I32(8),
        Value::Bool(true),
        Value::I32(8),
        Value::I32(4),
        Value::Array("y".into(), (0..16).map(Value::Byte).collect()),
    ]);
    let id = r.notify(0, "with image", &[], vec![("image-data", img), ("urgency", Value::Byte(2))], 0);
    assert!(id > 0);
    assert_eq!(r.state().2, 1);
}

#[test]
fn gdbus_can_notify_like_libnotify() {
    if Command::new("gdbus").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        return eprintln!("skip: no gdbus");
    }
    let Some(r) = rig() else { return eprintln!("skip") };
    let out = Command::new("gdbus")
        .args([
            "call",
            "--address",
            &r.d.address,
            "--dest",
            NOTIF_NAME,
            "--object-path",
            NOTIF_PATH,
            "--method",
            &format!("{NOTIF_IFACE}.Notify"),
            "gdbus-test",
            "0",
            "",
            "Hello 日本語",
            "A body",
            "['default', 'Open']",
            "{'urgency': <byte 1>, 'category': <'test'>}",
            "5000",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "(uint32 1,)");
    assert_eq!(r.state().2, 1);
}

#[test]
fn busctl_sees_both_interfaces() {
    if Command::new("busctl").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        return eprintln!("skip: no busctl");
    }
    let Some(r) = rig() else { return eprintln!("skip") };
    for (dest, path, needle) in [(NOTIF_NAME, NOTIF_PATH, "GetServerInformation"), (CTL_NAME, CTL_PATH, "ToggleDnd")] {
        let out = Command::new("busctl")
            .args(["--address", &r.d.address, "introspect", dest, path])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains(needle), "{text} {}", String::from_utf8_lossy(&out.stderr));
    }
    // property read
    let out = Command::new("busctl")
        .args(["--address", &r.d.address, "get-property", CTL_NAME, CTL_PATH, CTL_IFACE, "Count"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "u 0");
}

#[test]
fn a_second_notification_daemon_is_refused_with_a_helpful_message() {
    let Some(r) = rig() else { return eprintln!("skip") };
    let (other, _rx) = Conn::connect(&r.d.address).unwrap();
    let err = service::claim_names(&other).unwrap_err();
    assert!(err.contains("already running"), "{err}");

    // a foreign daemon that owns only the Notifications name
    let Some(d2) = Daemon::start() else { return };
    let (foreign, _frx) = Conn::connect(&d2.address).unwrap();
    assert_eq!(foreign.request_name(NOTIF_NAME, 4).unwrap(), 1);
    let (ours, _orx) = Conn::connect(&d2.address).unwrap();
    let err = service::claim_names(&ours).unwrap_err();
    assert!(err.contains("another notification daemon"), "{err}");
}
