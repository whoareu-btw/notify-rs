//! Test harness around tools/mock_compositor.py.
#![allow(dead_code)]

pub mod player;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

static N: AtomicUsize = AtomicUsize::new(0);

pub struct Mock {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    pub log: Vec<String>,
    pub dir: PathBuf,
    pub socket: PathBuf,
}

impl Mock {
    /// Start the mock compositor; None (with a note) when python3 or PIL is missing.
    pub fn start(args: &[&str]) -> Option<Mock> {
        let ok = Command::new("python3").args(["-c", "import PIL.Image"]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            eprintln!("skip: python3 with PIL is needed for the mock compositor");
            return None;
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dir = std::env::temp_dir().join(format!("notify-rs-mock-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dump")).unwrap();
        let mut child = Command::new("python3")
            .arg(root.join("tools/mock_compositor.py"))
            .arg(root.join("tests/fixtures/wayland_protocol.txt"))
            .args(args)
            .arg("--dump")
            .arg(dir.join("dump"))
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", "wl-test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the mock compositor");
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut m = Mock { child, stdin, rx, log: Vec::new(), socket: dir.join("wl-test"), dir };
        m.wait(0, "the mock to start", |l| l.starts_with("ready"));
        Some(m)
    }

    pub fn cmd(&mut self, c: &str) {
        writeln!(self.stdin, "{c}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn drain(&mut self) {
        while let Ok(l) = self.rx.try_recv() {
            self.log.push(l);
        }
    }

    /// Position in the log; later `wait`s can start from here.
    pub fn pos(&mut self) -> usize {
        self.drain();
        self.log.len()
    }

    /// The first line at or after `from` that satisfies `f`, waiting up to 5 s.
    pub fn wait(&mut self, from: usize, what: &str, f: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + Duration::from_secs(5);
        let mut i = from;
        loop {
            self.drain();
            while i < self.log.len() {
                if f(&self.log[i]) {
                    return self.log[i].clone();
                }
                i += 1;
            }
            if Instant::now() > end {
                panic!("timed out waiting for {what}; mock said:\n{}", self.log[from.min(self.log.len())..].join("\n"));
            }
            if let Ok(l) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.log.push(l);
            }
        }
    }

    pub fn lines(&mut self) -> Vec<String> {
        self.drain();
        self.log.clone()
    }

    /// Fail if the mock ever reported a protocol violation.
    pub fn assert_clean(&mut self) {
        std::thread::sleep(Duration::from_millis(100));
        self.drain();
        let bad: Vec<&String> = self.log.iter().filter(|l| l.starts_with("protocol_error") || l.starts_with("bad_command")).collect();
        assert!(bad.is_empty(), "the compositor objected: {bad:?}");
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"quit\n");
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `key=value` from a mock line.
pub fn kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ').find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}
