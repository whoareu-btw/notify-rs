//! Commands that talk to the running daemon (or become it).

use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::sleep;
use std::time::Duration;

use crate::dbus::{Conn, Error, Message, Value};
use crate::model::bell_json;
use crate::service::{self, CTL_IFACE, CTL_NAME, CTL_PATH, NOTIF_IFACE, NOTIF_NAME, NOTIF_PATH};

pub const HELP: &str = "    notify-rs daemon [--headless]  run the notification daemon and panel (Wayland session)
    notify-rs center [toggle|open|close]
                                   show or hide the notification center (default: toggle)
    notify-rs dnd [toggle|on|off]  Do Not Disturb (default: toggle)
    notify-rs clear                dismiss every notification
    notify-rs state                print: open=.. dnd=.. count=..
    notify-rs bell [--hide-empty]  stream waybar JSON for a notification bell
    notify-rs send [-u low|normal|critical] [-t MS] [-a APP] [-i ICON] [-r ID] [-p]
                   [-h TYPE:NAME:VALUE]... SUMMARY [BODY]
                                   send a notification (like notify-send)
";

/// Returns None when `cmd` is not one of ours.
pub fn run(cmd: &str, args: &[String]) -> Option<ExitCode> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    Some(match cmd {
        "daemon" => daemon(args),
        "center" | "panel" => match arg(1).unwrap_or("toggle") {
            m @ ("toggle" | "open" | "close") => control(&capitalize(m), vec![]),
            other => usage(&format!("center: expected toggle, open or close, got '{other}'")),
        },
        "dnd" => match arg(1).unwrap_or("toggle") {
            "toggle" => control("ToggleDnd", vec![]),
            "on" => control("SetDnd", vec![Value::Bool(true)]),
            "off" => control("SetDnd", vec![Value::Bool(false)]),
            other => usage(&format!("dnd: expected toggle, on or off, got '{other}'")),
        },
        "clear" => control("ClearAll", vec![]),
        "state" => state(),
        "bell" => bell(args.iter().any(|a| a == "--hide-empty")),
        "send" => send(&args[1..]),
        _ => return None,
    })
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("notify-rs: {msg}");
    ExitCode::from(2)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn connect() -> Result<(Conn, Receiver<Message>), ExitCode> {
    Conn::session().map_err(|e| {
        eprintln!("notify-rs: cannot reach the session bus: {e}");
        ExitCode::FAILURE
    })
}

fn daemon_call(conn: &Conn, member: &str, body: Vec<Value>) -> Result<Message, Error> {
    conn.call(Message::call(CTL_NAME, CTL_PATH, CTL_IFACE, member, body))
}

fn control(member: &str, body: Vec<Value>) -> ExitCode {
    let (conn, _rx) = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    match daemon_call(&conn, member, body) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => not_running(e),
    }
}

fn not_running(e: Error) -> ExitCode {
    match e {
        Error::Remote { ref name, .. }
            if name == "org.freedesktop.DBus.Error.ServiceUnknown" || name == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            eprintln!("notify-rs: the daemon is not running (start it with `notify-rs daemon`)");
        }
        e => eprintln!("notify-rs: {e}"),
    }
    ExitCode::FAILURE
}

fn read_state(conn: &Conn) -> Result<(bool, bool, u32), Error> {
    let r = daemon_call(conn, "GetState", vec![])?;
    let b = |i: usize| r.body.get(i).and_then(Value::as_bool).unwrap_or(false);
    Ok((b(0), b(1), r.body.get(2).and_then(Value::as_u32).unwrap_or(0)))
}

fn state() -> ExitCode {
    let (conn, _rx) = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    match read_state(&conn) {
        Ok((open, dnd, count)) => {
            println!("open={open} dnd={dnd} count={count}");
            ExitCode::SUCCESS
        }
        Err(e) => not_running(e),
    }
}

/// Waybar feed for the bell. Reconnects when the bus or the daemon goes away.
fn bell(hide_empty: bool) -> ExitCode {
    let stdout = std::io::stdout();
    let mut last = String::new();
    let mut emit = |line: String| -> bool {
        if line == last {
            return true;
        }
        let mut out = stdout.lock();
        let ok = writeln!(out, "{line}").is_ok() && out.flush().is_ok();
        last = line;
        ok
    };

    loop {
        let session = connect_quiet().and_then(|(c, rx)| {
            c.add_match(&format!("type='signal',interface='{CTL_IFACE}',member='StateChanged'"))?;
            c.add_match(&format!(
                "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{CTL_NAME}'"
            ))?;
            Ok((c, rx))
        });
        let (conn, rx) = match session {
            Ok(s) => s,
            Err(_) => {
                if !emit(bell_json(None, hide_empty)) {
                    return ExitCode::SUCCESS;
                }
                sleep(Duration::from_secs(2));
                continue;
            }
        };
        loop {
            let st = read_state(&conn).ok();
            if !emit(bell_json(st, hide_empty)) {
                return ExitCode::SUCCESS;
            }
            // wait for any signal, then merge a burst
            match rx.recv_timeout(Duration::from_secs(600)) {
                Ok(_) => while rx.recv_timeout(Duration::from_millis(30)).is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        sleep(Duration::from_secs(1));
    }
}

fn connect_quiet() -> Result<(Conn, Receiver<Message>), Error> {
    Conn::session()
}

// ---------------------------------------------------------------- send

#[derive(Debug, PartialEq)]
pub struct SendArgs {
    pub app: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: u8,
    pub timeout_ms: i32,
    pub replace: u32,
    pub print_id: bool,
    pub hints: Vec<(String, Value)>,
}

/// `TYPE:NAME:VALUE` as accepted by notify-send's `-h`.
pub fn parse_hint(s: &str) -> Option<(String, Value)> {
    let mut parts = s.splitn(3, ':');
    let (ty, name, val) = (parts.next()?, parts.next()?, parts.next()?);
    if name.is_empty() {
        return None;
    }
    let v = match ty {
        "int" | "int32" => Value::I32(val.parse().ok()?),
        "uint" | "uint32" => Value::U32(val.parse().ok()?),
        "byte" => Value::Byte(val.parse().ok()?),
        "double" => Value::F64(val.parse().ok()?),
        "boolean" | "bool" => Value::Bool(match val {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        }),
        "string" => Value::s(val),
        _ => return None,
    };
    Some((name.to_owned(), v))
}

pub fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let mut a = SendArgs {
        app: "notify-rs".into(),
        icon: String::new(),
        summary: String::new(),
        body: String::new(),
        urgency: 1,
        timeout_ms: -1,
        replace: 0,
        print_id: false,
        hints: Vec::new(),
    };
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    let need = |i: usize, flag: &str| -> Result<&String, String> {
        args.get(i + 1).ok_or_else(|| format!("{flag} needs a value"))
    };
    while i < args.len() {
        let f = args[i].as_str();
        match f {
            "-u" | "--urgency" => {
                a.urgency = match need(i, f)?.as_str() {
                    "low" => 0,
                    "normal" => 1,
                    "critical" => 2,
                    other => return Err(format!("unknown urgency '{other}' (low, normal or critical)")),
                };
                i += 2;
            }
            "-t" | "--expire-time" => {
                a.timeout_ms = need(i, f)?.parse().map_err(|_| "expire time must be a number of milliseconds".to_string())?;
                i += 2;
            }
            "-a" | "--app-name" => {
                a.app = need(i, f)?.clone();
                i += 2;
            }
            "-i" | "--icon" => {
                a.icon = need(i, f)?.clone();
                i += 2;
            }
            "-r" | "--replace-id" => {
                a.replace = need(i, f)?.parse().map_err(|_| "replace id must be a number".to_string())?;
                i += 2;
            }
            "-h" | "--hint" => {
                let h = need(i, f)?;
                a.hints.push(parse_hint(h).ok_or_else(|| format!("bad hint '{h}' (expected TYPE:NAME:VALUE, e.g. int:value:50)"))?);
                i += 2;
            }
            "-p" | "--print-id" => {
                a.print_id = true;
                i += 1;
            }
            "--" => {
                positional.extend(args[i + 1..].iter().map(String::as_str));
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option '{s}'")),
            s => {
                positional.push(s);
                i += 1;
            }
        }
    }
    match positional.as_slice() {
        [] => return Err("a summary is required".into()),
        [s] => a.summary = s.to_string(),
        [s, b] => {
            a.summary = s.to_string();
            a.body = b.to_string();
        }
        _ => return Err("too many arguments (summary and body only; quote them)".into()),
    }
    a.hints.insert(0, ("urgency".into(), Value::Byte(a.urgency)));
    Ok(a)
}

fn send(args: &[String]) -> ExitCode {
    let a = match parse_send_args(args) {
        Ok(a) => a,
        Err(e) => return usage(&format!("send: {e}")),
    };
    let (conn, _rx) = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let r = conn.call(Message::call(
        NOTIF_NAME,
        NOTIF_PATH,
        NOTIF_IFACE,
        "Notify",
        vec![
            Value::s(a.app),
            Value::U32(a.replace),
            Value::s(a.icon),
            Value::s(a.summary),
            Value::s(a.body),
            Value::strings(Vec::<String>::new()),
            Value::dict_sv(a.hints),
            Value::I32(a.timeout_ms),
        ],
    ));
    match r {
        Ok(m) => {
            if a.print_id {
                println!("{}", m.body.first().and_then(Value::as_u32).unwrap_or(0));
            }
            ExitCode::SUCCESS
        }
        Err(Error::Remote { name, .. }) if name == "org.freedesktop.DBus.Error.ServiceUnknown" => {
            eprintln!("notify-rs: no notification daemon is running (start it with `notify-rs daemon`)");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("notify-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------- daemon

fn daemon(args: &[String]) -> ExitCode {
    let headless = args.iter().any(|a| a == "--headless");
    let (conn, rx) = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    if headless {
        if let Err(e) = service::claim_names(&conn) {
            eprintln!("notify-rs: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!("notify-rs: running without a window (--headless)");
        service::run_headless(service::Service::new(conn), rx);
        eprintln!("notify-rs: lost the session bus");
        return ExitCode::FAILURE;
    }
    // the window opens (or fails) before we take the notification name, so a
    // broken display setup never leaves notifications going nowhere
    match crate::ui::app::run(conn, rx) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("notify-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn send_parsing() {
        let a = parse_send_args(&v(&["-u", "critical", "-t", "3000", "-a", "app", "-i", "mail", "-r", "7", "-p", "Hi", "There"])).unwrap();
        assert_eq!((a.urgency, a.timeout_ms, a.replace), (2, 3000, 7));
        assert_eq!((a.app.as_str(), a.icon.as_str(), a.summary.as_str(), a.body.as_str()), ("app", "mail", "Hi", "There"));
        assert!(a.print_id);
        assert_eq!(a.hints[0], ("urgency".to_string(), Value::Byte(2)));
    }

    #[test]
    fn send_defaults_and_errors() {
        let a = parse_send_args(&v(&["Only summary"])).unwrap();
        assert_eq!((a.urgency, a.timeout_ms, a.body.as_str()), (1, -1, ""));
        assert!(parse_send_args(&v(&[])).is_err());
        assert!(parse_send_args(&v(&["a", "b", "c"])).is_err());
        assert!(parse_send_args(&v(&["-u", "loud", "x"])).is_err());
        assert!(parse_send_args(&v(&["-t", "soon", "x"])).is_err());
        assert!(parse_send_args(&v(&["-t"])).is_err());
        assert!(parse_send_args(&v(&["--bogus", "x"])).is_err());
        let a = parse_send_args(&v(&["--", "-dash summary"])).unwrap();
        assert_eq!(a.summary, "-dash summary");
    }

    #[test]
    fn hints() {
        assert_eq!(parse_hint("int:value:50"), Some(("value".into(), Value::I32(50))));
        assert_eq!(parse_hint("string:x-canonical-private-synchronous:vol"), Some(("x-canonical-private-synchronous".into(), Value::s("vol"))));
        assert_eq!(parse_hint("boolean:resident:true"), Some(("resident".into(), Value::Bool(true))));
        assert_eq!(parse_hint("string:a:b:c"), Some(("a".into(), Value::s("b:c"))), "value may contain colons");
        assert_eq!(parse_hint("int:value:x"), None);
        assert_eq!(parse_hint("weird:a:b"), None);
        assert_eq!(parse_hint("int::5"), None);
        assert_eq!(parse_hint("int"), None);
        let a = parse_send_args(&v(&["-h", "int:value:30", "-h", "byte:urgency:0", "x"])).unwrap();
        assert_eq!(a.hints.len(), 3);
    }
}
