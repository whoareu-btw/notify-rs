//! A blocking D-Bus connection with one reader thread.
//!
//! * `call` sends a method call and waits for its reply (any thread).
//! * Everything that is not a reply (signals, method calls addressed to us)
//!   goes to the `Receiver<Message>` returned by `connect`.
//! * When the bus goes away the receiver disconnects.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::message::{Message, MsgType};
use super::value::Value;
use super::Error;

pub const BUS: &str = "org.freedesktop.DBus";
pub const BUS_PATH: &str = "/org/freedesktop/DBus";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

// RequestName flags / replies
pub const NAME_DO_NOT_QUEUE: u32 = 4;
pub const NAME_REPLY_PRIMARY_OWNER: u32 = 1;
pub const NAME_REPLY_ALREADY_OWNER: u32 = 4;

// ---------------------------------------------------------------- addresses

#[derive(Debug, PartialEq)]
pub enum Addr {
    Path(PathBuf),
    Abstract(Vec<u8>),
}

fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(x) = h {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Parse a D-Bus address list (`unix:path=/run/user/1000/bus;unix:abstract=...`).
/// Transports other than `unix` are skipped.
pub fn parse_addresses(s: &str) -> Vec<Addr> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for entry in s.split(';') {
        let Some((transport, rest)) = entry.split_once(':') else { continue };
        if transport != "unix" {
            continue;
        }
        for kv in rest.split(',') {
            match kv.split_once('=') {
                Some(("path", v)) => {
                    out.push(Addr::Path(PathBuf::from(std::ffi::OsString::from_vec(unescape(v)))));
                    break;
                }
                Some(("abstract", v)) => {
                    out.push(Addr::Abstract(unescape(v)));
                    break;
                }
                _ => {}
            }
        }
    }
    out
}

/// Where the session bus is: `DBUS_SESSION_BUS_ADDRESS`, else the standard socket.
pub fn session_addresses() -> Vec<Addr> {
    if let Ok(a) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        let v = parse_addresses(&a);
        if !v.is_empty() {
            return v;
        }
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", crate::sys::uid())));
    vec![Addr::Path(dir.join("bus"))]
}

fn open(addr: &Addr) -> std::io::Result<UnixStream> {
    match addr {
        Addr::Path(p) => UnixStream::connect(p),
        Addr::Abstract(name) => UnixStream::connect_addr(&SocketAddr::from_abstract_name(name)?),
    }
}

// ---------------------------------------------------------------- connection

struct Shared {
    writer: Mutex<UnixStream>,
    serial: AtomicU32,
    pending: Mutex<HashMap<u32, Sender<Message>>>,
    closed: AtomicBool,
    unique: Mutex<String>,
}

#[derive(Clone)]
pub struct Conn {
    shared: Arc<Shared>,
}

impl Conn {
    /// Connect to the session bus.
    pub fn session() -> Result<(Conn, Receiver<Message>), Error> {
        let mut last = Error::Protocol("no session bus address".into());
        for a in session_addresses() {
            match Conn::connect_addr(&a) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Connect to a specific address string (used by tests).
    pub fn connect(address: &str) -> Result<(Conn, Receiver<Message>), Error> {
        let mut last = Error::Protocol("no usable address".into());
        for a in parse_addresses(address) {
            match Conn::connect_addr(&a) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn connect_addr(addr: &Addr) -> Result<(Conn, Receiver<Message>), Error> {
        let mut stream = open(addr)?;
        authenticate(&mut stream)?;

        let reader = stream.try_clone()?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(stream),
            serial: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            unique: Mutex::new(String::new()),
        });
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("dbus-reader".into())
            .spawn(move || reader_loop(reader, sh, tx))?;

        let conn = Conn { shared };
        let reply = conn.call(Message::call(BUS, BUS_PATH, BUS, "Hello", vec![]))?;
        let name = reply.body.first().and_then(Value::as_str).unwrap_or("").to_owned();
        *conn.shared.unique.lock().unwrap() = name;
        Ok((conn, rx))
    }

    /// Our unique name on the bus (`:1.42`).
    pub fn unique_name(&self) -> String {
        self.shared.unique.lock().unwrap().clone()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    fn next_serial(&self) -> u32 {
        loop {
            let s = self.shared.serial.fetch_add(1, Ordering::Relaxed);
            if s != 0 {
                return s;
            }
        }
    }

    fn write_msg(&self, msg: &Message) -> Result<(), Error> {
        let bytes = msg.encode()?;
        let mut w = self.shared.writer.lock().unwrap();
        w.write_all(&bytes)?;
        w.flush()?;
        Ok(())
    }

    /// Send without waiting. Returns the serial that was used.
    pub fn send(&self, mut msg: Message) -> Result<u32, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        msg.serial = self.next_serial();
        self.write_msg(&msg)?;
        Ok(msg.serial)
    }

    /// Method call with the default 5 s timeout.
    pub fn call(&self, msg: Message) -> Result<Message, Error> {
        self.call_for(msg, DEFAULT_TIMEOUT)
    }

    pub fn call_for(&self, mut msg: Message, timeout: Duration) -> Result<Message, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        msg.flags &= !super::message::NO_REPLY_EXPECTED;
        let (tx, rx) = mpsc::channel();
        // Register the waiter before writing so a fast reply cannot be missed.
        let serial = self.next_serial();
        msg.serial = serial;
        self.shared.pending.lock().unwrap().insert(serial, tx);
        if let Err(e) = self.write_msg(&msg) {
            self.shared.pending.lock().unwrap().remove(&serial);
            return Err(e);
        }
        let result = rx.recv_timeout(timeout);
        self.shared.pending.lock().unwrap().remove(&serial);
        match result {
            Ok(reply) => match reply.mtype {
                MsgType::Error => Err(Error::Remote {
                    name: reply.error_name.clone().unwrap_or_default(),
                    text: reply.body.first().and_then(Value::as_str).unwrap_or("").to_owned(),
                }),
                _ => Ok(reply),
            },
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    /// Reply to a method call received on the inbox.
    pub fn reply(&self, call: &Message, body: Vec<Value>) -> Result<(), Error> {
        if !call.expects_reply() {
            return Ok(());
        }
        self.send(Message::reply_to(call, body)).map(|_| ())
    }

    pub fn reply_error(&self, call: &Message, name: &str, text: &str) -> Result<(), Error> {
        if !call.expects_reply() {
            return Ok(());
        }
        self.send(Message::error_to(call, name, text)).map(|_| ())
    }

    pub fn emit(&self, path: &str, iface: &str, member: &str, body: Vec<Value>) -> Result<(), Error> {
        self.send(Message::signal(path, iface, member, body)).map(|_| ())
    }

    /// Answer `org.freedesktop.DBus.Peer` calls. Returns true if `msg` was one.
    pub fn handle_peer(&self, msg: &Message) -> bool {
        if msg.interface.as_deref() != Some("org.freedesktop.DBus.Peer") {
            return false;
        }
        match msg.member.as_deref() {
            Some("Ping") => {
                let _ = self.reply(msg, vec![]);
            }
            Some("GetMachineId") => {
                let id = std::fs::read_to_string("/etc/machine-id")
                    .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
                    .unwrap_or_default();
                let id = id.trim();
                let id = if id.len() == 32 { id } else { "00000000000000000000000000000000" };
                let _ = self.reply(msg, vec![Value::s(id)]);
            }
            _ => {
                let _ = self.reply_error(msg, "org.freedesktop.DBus.Error.UnknownMethod", "unknown method");
            }
        }
        true
    }

    // ---- bus-level helpers

    pub fn request_name(&self, name: &str, flags: u32) -> Result<u32, Error> {
        let r = self.call(Message::call(
            BUS,
            BUS_PATH,
            BUS,
            "RequestName",
            vec![Value::s(name), Value::U32(flags)],
        ))?;
        r.body.first().and_then(Value::as_u32).ok_or_else(|| Error::Protocol("bad RequestName reply".into()))
    }

    pub fn add_match(&self, rule: &str) -> Result<(), Error> {
        self.call(Message::call(BUS, BUS_PATH, BUS, "AddMatch", vec![Value::s(rule)])).map(|_| ())
    }

    pub fn list_names(&self) -> Result<Vec<String>, Error> {
        let r = self.call(Message::call(BUS, BUS_PATH, BUS, "ListNames", vec![]))?;
        Ok(r.body.first().map(Value::as_strings).unwrap_or_default())
    }

    /// Unique name that owns `name`, if anyone does.
    pub fn name_owner(&self, name: &str) -> Result<Option<String>, Error> {
        match self.call(Message::call(BUS, BUS_PATH, BUS, "GetNameOwner", vec![Value::s(name)])) {
            Ok(r) => Ok(r.body.first().and_then(Value::as_str).map(str::to_owned)),
            Err(Error::Remote { name, .. }) if name == "org.freedesktop.DBus.Error.NameHasNoOwner" => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn get_all(&self, dest: &str, path: &str, iface: &str) -> Result<Value, Error> {
        let r = self.call(Message::call(
            dest,
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            vec![Value::s(iface)],
        ))?;
        r.body.into_iter().next().ok_or_else(|| Error::Protocol("empty GetAll reply".into()))
    }

    pub fn get_prop(&self, dest: &str, path: &str, iface: &str, prop: &str) -> Result<Value, Error> {
        let r = self.call(Message::call(
            dest,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            vec![Value::s(iface), Value::s(prop)],
        ))?;
        r.body.into_iter().next().map(|v| v.inner().clone()).ok_or_else(|| Error::Protocol("empty Get reply".into()))
    }

    pub fn set_prop(&self, dest: &str, path: &str, iface: &str, prop: &str, value: Value) -> Result<(), Error> {
        self.call(Message::call(
            dest,
            path,
            "org.freedesktop.DBus.Properties",
            "Set",
            vec![Value::s(iface), Value::s(prop), Value::v(value)],
        ))
        .map(|_| ())
    }

    /// Close the socket; the reader thread then ends and the inbox disconnects.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        if let Ok(w) = self.shared.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn read_line(stream: &mut UnixStream) -> Result<String, Error> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 1024 {
        match stream.read(&mut b) {
            Ok(0) => return Err(Error::Closed),
            Ok(_) => {
                line.push(b[0]);
                if line.ends_with(b"\r\n") {
                    line.truncate(line.len() - 2);
                    return Ok(String::from_utf8_lossy(&line).into_owned());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Protocol("auth line too long".into()))
}

fn authenticate(stream: &mut UnixStream) -> Result<(), Error> {
    let uid = crate::sys::uid().to_string();
    let mut hello = vec![0u8];
    hello.extend_from_slice(format!("AUTH EXTERNAL {}\r\n", hex(&uid)).as_bytes());
    stream.write_all(&hello)?;
    let reply = read_line(stream)?;
    if !reply.starts_with("OK") {
        return Err(Error::Protocol(format!("authentication refused: {reply}")));
    }
    stream.write_all(b"BEGIN\r\n")?;
    Ok(())
}

fn reader_loop(mut stream: UnixStream, shared: Arc<Shared>, inbox: Sender<Message>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 64 * 1024];
    'outer: loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        loop {
            match Message::decode(&buf) {
                Ok(Some((msg, used))) => {
                    buf.drain(..used);
                    match msg.mtype {
                        MsgType::MethodReturn | MsgType::Error => {
                            if let Some(rs) = msg.reply_serial {
                                let tx = shared.pending.lock().unwrap().remove(&rs);
                                if let Some(tx) = tx {
                                    let _ = tx.send(msg);
                                }
                            }
                        }
                        MsgType::Other(_) => {}
                        _ => {
                            // if nobody listens any more, keep draining the socket
                            let _ = inbox.send(msg);
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break 'outer, // protocol error: the stream cannot be resynchronised
            }
        }
        if buf.capacity() > 1 << 20 && buf.len() < 64 * 1024 {
            buf.shrink_to(64 * 1024);
        }
    }
    shared.closed.store(true, Ordering::Relaxed);
    // wake every blocked caller
    shared.pending.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses() {
        assert_eq!(
            parse_addresses("unix:path=/run/user/1000/bus"),
            vec![Addr::Path("/run/user/1000/bus".into())]
        );
        assert_eq!(
            parse_addresses("unix:path=/run/user/1000/bus,guid=abc"),
            vec![Addr::Path("/run/user/1000/bus".into())]
        );
        assert_eq!(
            parse_addresses("unix:guid=abc,abstract=/tmp/dbus-XYZ"),
            vec![Addr::Abstract(b"/tmp/dbus-XYZ".to_vec())]
        );
        assert_eq!(
            parse_addresses("tcp:host=x,port=1;unix:path=/a%20b;unix:abstract=n"),
            vec![Addr::Path("/a b".into()), Addr::Abstract(b"n".to_vec())]
        );
        assert!(parse_addresses("").is_empty());
        assert!(parse_addresses("nonsense").is_empty());
        // a trailing '%' or a bad escape must not panic
        let _ = parse_addresses("unix:path=/x%");
        let _ = parse_addresses("unix:path=/x%zz");
        let _ = parse_addresses("unix:path=/x%4");
    }

    #[test]
    fn hex_of_uid() {
        assert_eq!(hex("1000"), "31303030");
        assert_eq!(hex("0"), "30");
    }
}
