//! Talks to a real `dbus-daemon` (started per test) and cross-checks the wire
//! format against busctl and gdbus. Skipped when those tools are missing.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use notify_rs::dbus::{Conn, Message, MsgType, Value};

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
        if address.is_empty() {
            return None;
        }
        Some(Daemon { child, address })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

const SVC: &str = "dev.notifyrs.Test";
const OBJ: &str = "/dev/notifyrs/Test";
const IFACE: &str = "dev.notifyrs.Test";

/// Echo server: every call to `Echo` returns its arguments unchanged.
/// `Boom` returns an error. `Add(ii)` returns the sum.
fn serve(conn: Conn, rx: Receiver<Message>) {
    std::thread::spawn(move || {
        while let Ok(m) = rx.recv() {
            if m.mtype != MsgType::MethodCall {
                continue;
            }
            if conn.handle_peer(&m) {
                continue;
            }
            match m.member.as_deref() {
                Some("Echo") => {
                    let _ = conn.reply(&m, m.body.clone());
                }
                Some("Add") => {
                    let a = m.body.first().and_then(Value::as_i64).unwrap_or(0);
                    let b = m.body.get(1).and_then(Value::as_i64).unwrap_or(0);
                    let _ = conn.reply(&m, vec![Value::I32((a + b) as i32)]);
                }
                Some("Boom") => {
                    let _ = conn.reply_error(&m, "dev.notifyrs.Test.Error.Boom", "kaboom");
                }
                _ => {
                    let _ = conn.reply_error(&m, "org.freedesktop.DBus.Error.UnknownMethod", "no such method");
                }
            }
        }
    });
}

fn start() -> Option<(Daemon, Conn)> {
    let d = Daemon::start()?;
    let (c, rx) = Conn::connect(&d.address).expect("connect");
    assert_eq!(c.request_name(SVC, 4).unwrap(), 1);
    serve(c.clone(), rx);
    Some((d, c))
}

#[test]
fn hello_and_names() {
    let Some(d) = Daemon::start() else { return eprintln!("skip: no dbus-daemon") };
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    assert!(c.unique_name().starts_with(':'), "{}", c.unique_name());
    let names = c.list_names().unwrap();
    assert!(names.iter().any(|n| n == "org.freedesktop.DBus"));
    assert_eq!(c.name_owner("does.not.exist").unwrap(), None);
    assert_eq!(c.request_name("dev.notifyrs.A", 4).unwrap(), 1);
    assert_eq!(c.request_name("dev.notifyrs.A", 4).unwrap(), 4, "already owner");
    assert_eq!(c.name_owner("dev.notifyrs.A").unwrap(), Some(c.unique_name()));
}

#[test]
fn second_owner_is_refused_with_do_not_queue() {
    let Some((d, _c)) = start() else { return eprintln!("skip") };
    let (c2, _rx2) = Conn::connect(&d.address).unwrap();
    assert_eq!(c2.request_name(SVC, 4).unwrap(), 3, "EXISTS");
}

#[test]
fn rust_to_rust_roundtrip_of_complex_values() {
    let Some((d, _srv)) = start() else { return eprintln!("skip") };
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    let body = vec![
        Value::Byte(7),
        Value::Bool(true),
        Value::I16(-2),
        Value::U16(2),
        Value::I32(-3),
        Value::U32(3),
        Value::I64(-4),
        Value::U64(4),
        Value::F64(5.5),
        Value::s("日本語 🎵"),
        Value::path("/a/b"),
        Value::Sig("a{sv}".into()),
        Value::strings(["x", "y", "z"]),
        Value::dict_sv(vec![
            ("title".into(), Value::s("曲")),
            ("len".into(), Value::I64(1 << 40)),
            ("artists".into(), Value::strings(["a"])),
            ("pos".into(), Value::Struct(vec![Value::I32(1), Value::s("s")])),
        ]),
        Value::Array("(ayi)".into(), vec![Value::Struct(vec![Value::Array("y".into(), vec![Value::Byte(1)]), Value::I32(2)])]),
        Value::v(Value::v(Value::U32(9))),
        Value::Array("y".into(), vec![Value::Byte(0xab); 200_000]),
    ];
    let r = c.call(Message::call(SVC, OBJ, IFACE, "Echo", body.clone())).unwrap();
    assert_eq!(r.body, body);
}

#[test]
fn errors_come_back_as_remote_errors() {
    let Some((d, _srv)) = start() else { return eprintln!("skip") };
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    match c.call(Message::call(SVC, OBJ, IFACE, "Boom", vec![])) {
        Err(notify_rs::dbus::Error::Remote { name, text }) => {
            assert_eq!(name, "dev.notifyrs.Test.Error.Boom");
            assert_eq!(text, "kaboom");
        }
        other => panic!("{other:?}"),
    }
    match c.call(Message::call("dev.notifyrs.Nobody", OBJ, IFACE, "X", vec![])) {
        Err(notify_rs::dbus::Error::Remote { name, .. }) => {
            assert_eq!(name, "org.freedesktop.DBus.Error.ServiceUnknown")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn many_concurrent_calls_get_the_right_replies() {
    let Some((d, _srv)) = start() else { return eprintln!("skip") };
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    let mut handles = Vec::new();
    for t in 0..8 {
        let c = c.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..100 {
                let r = c
                    .call(Message::call(SVC, OBJ, IFACE, "Add", vec![Value::I32(t * 1000), Value::I32(i)]))
                    .unwrap();
                assert_eq!(r.body[0].as_i64(), Some((t * 1000 + i) as i64));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn signals_are_delivered_to_matching_listeners() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let (a, rx_a) = Conn::connect(&d.address).unwrap();
    let (b, _rx_b) = Conn::connect(&d.address).unwrap();
    a.add_match("type='signal',interface='dev.notifyrs.Sig'").unwrap();
    b.emit("/x", "dev.notifyrs.Sig", "Ping", vec![Value::s("hello"), Value::U32(5)]).unwrap();
    // the bus itself also sends us NameAcquired etc.; skip to ours
    let m = loop {
        let m = rx_a.recv_timeout(Duration::from_secs(3)).unwrap();
        if m.interface.as_deref() == Some("dev.notifyrs.Sig") {
            break m;
        }
    };
    assert!(m.is_signal("dev.notifyrs.Sig", "Ping"));
    assert_eq!(m.sender.as_deref(), Some(b.unique_name().as_str()));
    assert_eq!(m.body[0].as_str(), Some("hello"));
    assert_eq!(m.body[1].as_u32(), Some(5));
}

#[test]
fn call_times_out_and_close_wakes_everyone() {
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let (silent, rx_silent) = Conn::connect(&d.address).unwrap();
    silent.request_name("dev.notifyrs.Silent", 4).unwrap();
    let _keep = rx_silent; // never answers
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    let r = c.call_for(Message::call("dev.notifyrs.Silent", "/", "a.B", "C", vec![]), Duration::from_millis(200));
    assert!(matches!(r, Err(notify_rs::dbus::Error::Timeout)), "{r:?}");
    c.close();
    assert!(c.call(Message::call("x.y", "/", "a.B", "C", vec![])).is_err());
}

#[test]
fn inbox_disconnects_when_the_bus_dies() {
    let Some(mut d) = Daemon::start() else { return eprintln!("skip") };
    let (c, rx) = Conn::connect(&d.address).unwrap();
    let _ = d.child.kill();
    let _ = d.child.wait();
    // drain whatever the bus sent before it died, then the channel must report Disconnected
    let end = loop {
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(matches!(end, std::sync::mpsc::RecvTimeoutError::Disconnected), "{end:?}");
    assert!(c.is_closed());
}

#[test]
fn busctl_can_call_us_and_sees_our_types() {
    if !have("busctl") {
        return eprintln!("skip: no busctl");
    }
    let Some((d, _srv)) = start() else { return eprintln!("skip") };
    let out = Command::new("busctl")
        .args(["--address", &d.address, "call", SVC, OBJ, IFACE, "Echo", "sua{sv}as", "héllo", "42", "2", "a", "s", "x", "b", "b", "true", "2", "p", "q"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{} {}", text, String::from_utf8_lossy(&out.stderr));
    assert!(text.starts_with("sua{sv}as "), "{text}");
    // busctl prints non-ASCII bytes as octal escapes: \303\251 is the UTF-8 for é
    assert!(text.contains("h\\303\\251llo") || text.contains("héllo"), "{text}");
    assert!(text.contains("42"), "{text}");

    let out = Command::new("busctl")
        .args(["--address", &d.address, "call", SVC, OBJ, IFACE, "Add", "ii", "40", "2"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "i 42");

    // peer interface
    let out = Command::new("busctl")
        .args(["--address", &d.address, "call", SVC, OBJ, "org.freedesktop.DBus.Peer", "Ping"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // error text arrives
    let out = Command::new("busctl")
        .args(["--address", &d.address, "call", SVC, OBJ, IFACE, "Boom"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("kaboom"));
}

#[test]
fn gdbus_can_call_us_too() {
    if !have("gdbus") {
        return eprintln!("skip: no gdbus");
    }
    let Some((d, _srv)) = start() else { return eprintln!("skip") };
    let out = Command::new("gdbus")
        .args(["call", "--address", &d.address, "--dest", SVC, "--object-path", OBJ, "--method", &format!("{IFACE}.Add"), "40", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "(42,)");
}

#[test]
fn we_can_call_other_services_and_read_properties() {
    // The bus itself has properties: org.freedesktop.DBus Features / Interfaces.
    let Some(d) = Daemon::start() else { return eprintln!("skip") };
    let (c, _rx) = Conn::connect(&d.address).unwrap();
    let all = c.get_all("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").unwrap();
    assert!(all.dict_get("Features").is_some(), "{all:?}");
    let f = c.get_prop("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Features").unwrap();
    assert!(f.as_array().is_some());
}
