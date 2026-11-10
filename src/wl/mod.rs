//! A Wayland client written from the wire protocol up: connection, object ids,
//! shared-memory buffers, layer-shell surfaces and pointer input.

pub mod client;
pub mod proto;
pub mod wire;

use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use wire::{Args, Msg};

/// What the reader thread delivers.
#[derive(Debug)]
pub enum Incoming {
    Msg(Msg),
    /// The compositor went away or sent garbage.
    Closed(String),
}

/// `$WAYLAND_DISPLAY` (absolute, or relative to `$XDG_RUNTIME_DIR`; default `wayland-0`).
pub fn socket_path() -> Result<PathBuf, String> {
    socket_path_from(std::env::var("WAYLAND_DISPLAY").ok(), std::env::var("XDG_RUNTIME_DIR").ok())
}

pub fn socket_path_from(display: Option<String>, runtime: Option<String>) -> Result<PathBuf, String> {
    let display = display.filter(|d| !d.is_empty()).unwrap_or_else(|| "wayland-0".into());
    if display.starts_with('/') {
        return Ok(PathBuf::from(display));
    }
    match runtime.filter(|d| !d.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir).join(display)),
        None => Err("XDG_RUNTIME_DIR is not set, so the Wayland socket cannot be found".into()),
    }
}

/// The write half of the connection plus object-id bookkeeping. A thread reads
/// the other half and forwards whole messages.
pub struct Conn {
    sock: UnixStream,
    next: u32,
    free: Vec<u32>,
}

impl Conn {
    pub fn connect(path: &std::path::Path, tx: Sender<Incoming>) -> io::Result<Conn> {
        let sock = UnixStream::connect(path)?;
        let reader = sock.try_clone()?;
        std::thread::Builder::new().name("wayland-read".into()).spawn(move || read_loop(reader, tx))?;
        Ok(Conn { sock, next: 2, free: Vec::new() })
    }

    /// A fresh client-side object id (ids are recycled after `delete_id`).
    pub fn alloc(&mut self) -> u32 {
        if let Some(id) = self.free.pop() {
            return id;
        }
        let id = self.next;
        self.next += 1;
        id
    }

    /// The compositor says it no longer knows `id`.
    pub fn recycle(&mut self, id: u32) {
        if id >= 2 && !self.free.contains(&id) {
            self.free.push(id);
        }
    }

    pub fn send(&mut self, obj: u32, op: u16, args: Args) -> io::Result<()> {
        (&self.sock).write_all(&wire::encode(obj, op, &args))
    }

    /// Send a request that carries a file descriptor.
    pub fn send_fd(&mut self, obj: u32, op: u16, args: Args, fd: RawFd) -> io::Result<()> {
        crate::sys::send_with_fd(&self.sock, &wire::encode(obj, op, &args), fd)
    }

    pub fn shutdown(&self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

fn read_loop(mut sock: UnixStream, tx: Sender<Incoming>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match sock.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(Incoming::Closed("the compositor closed the connection".into()));
                return;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = tx.send(Incoming::Closed(format!("reading from the compositor failed: {e}")));
                return;
            }
        }
        let mut used = 0;
        loop {
            match wire::next_msg(&buf[used..]) {
                Ok(Some((m, n))) => {
                    used += n;
                    if tx.send(Incoming::Msg(m)).is_err() {
                        return; // nobody is listening any more
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Incoming::Closed(e));
                    return;
                }
            }
        }
        buf.drain(..used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_rules() {
        let p = |d: Option<&str>, r: Option<&str>| socket_path_from(d.map(String::from), r.map(String::from));
        assert_eq!(p(Some("wayland-1"), Some("/run/user/1000")).unwrap(), PathBuf::from("/run/user/1000/wayland-1"));
        assert_eq!(p(None, Some("/run/user/1000")).unwrap(), PathBuf::from("/run/user/1000/wayland-0"));
        assert_eq!(p(Some("/tmp/sock"), None).unwrap(), PathBuf::from("/tmp/sock"));
        assert!(p(Some("wayland-0"), None).is_err());
        assert!(p(Some("wayland-0"), Some("")).is_err());
    }

    #[test]
    fn ids_are_recycled_but_never_zero_or_display() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join(format!("notify-rs-wl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut c = Conn::connect(&path, tx).unwrap();
        let _ = l.accept().unwrap();
        assert_eq!((c.alloc(), c.alloc(), c.alloc()), (2, 3, 4));
        c.recycle(3);
        c.recycle(3);
        c.recycle(1); // the display is never reused
        assert_eq!(c.alloc(), 3);
        assert_eq!(c.alloc(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reader_delivers_messages_split_across_reads_and_reports_close() {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join(format!("notify-rs-wl2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let _c = Conn::connect(&path, tx).unwrap();
        let (mut srv, _) = l.accept().unwrap();
        let m1 = wire::encode(1, 0, &Args::new().u(11).s("wl_shm").u(1));
        let m2 = wire::encode(5, 2, &Args::new().i(-7));
        let mut all = m1.clone();
        all.extend_from_slice(&m2);
        for chunk in all.chunks(5) {
            srv.write_all(chunk).unwrap();
            srv.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let t = std::time::Duration::from_secs(2);
        match rx.recv_timeout(t).unwrap() {
            Incoming::Msg(m) => assert_eq!((m.obj, m.op), (1, 0)),
            other => panic!("{other:?}"),
        }
        match rx.recv_timeout(t).unwrap() {
            Incoming::Msg(m) => assert_eq!((m.obj, m.op), (5, 2)),
            other => panic!("{other:?}"),
        }
        drop(srv);
        assert!(matches!(rx.recv_timeout(t).unwrap(), Incoming::Closed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
