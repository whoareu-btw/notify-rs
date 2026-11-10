//! `art::fetch` against a throwaway local HTTP server, with the real `curl`.
//! One test function: it sets process-wide environment variables.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

const SERVER: &str = r#"
import http.server, sys
png = open(sys.argv[1], 'rb').read()
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path == '/ok.png':
            self.send(200, 'image/png', png)
        elif self.path == '/text.png':
            self.send(200, 'image/png', b'this is not an image' * 50)
        elif self.path == '/big.png':
            self.send(200, 'image/png', png + b'\0' * (20 * 1024 * 1024))
        elif self.path == '/redirect-file':
            self.send_response(302); self.send_header('Location', 'file:///etc/passwd'); self.end_headers()
        elif self.path == '/redirect-ok':
            self.send_response(302); self.send_header('Location', '/ok.png'); self.end_headers()
        else:
            self.send(404, 'text/plain', b'no')
    def send(self, code, ctype, body):
        self.send_response(code)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try: self.wfile.write(body)
        except Exception: pass
s = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
print(s.server_address[1], flush=True)
s.serve_forever()
"#;

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn downloads_validates_caches_and_refuses_the_rest() {
    if !have("python3", "--version") || !have("curl", "--version") {
        eprintln!("skip: needs python3 and curl");
        return;
    }
    let dir = std::env::temp_dir().join(format!("notify-rs-fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let png_path = dir.join("cover.png");
    std::fs::write(&png_path, notify_rs::gfx::png_write::encode_rgba(8, 8, &[10, 200, 30, 255].repeat(64))).unwrap();
    let cache = dir.join("cache");

    let mut child = Command::new("python3").arg("-c").arg(SERVER).arg(&png_path).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut port = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut port).unwrap();
    let _server = Server(child);
    let base = format!("http://127.0.0.1:{}", port.trim());

    // SAFETY: this is the only test in the process
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", &cache);
        std::env::set_var("NO_PROXY", "127.0.0.1");
        std::env::set_var("no_proxy", "127.0.0.1");
        for v in ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"] {
            std::env::remove_var(v);
        }
    }
    use notify_rs::art;
    let parts = || std::fs::read_dir(cache.join("notify-rs")).map(|d| d.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "part")).count()).unwrap_or(0);

    // a good image is downloaded into the cache
    let url = format!("{base}/ok.png");
    assert!(art::local_path(&url).is_none());
    let p = art::fetch(&url).expect("download");
    assert!(p.starts_with(cache.join("notify-rs")) && p.exists());
    assert_eq!(notify_rs::image::dims(&std::fs::read(&p).unwrap()), Some((8, 8)));
    assert_eq!(art::local_path(&url), Some(p.clone()), "now found without the network");

    // redirects inside http are followed
    assert!(art::fetch(&format!("{base}/redirect-ok")).is_some());

    // load_square goes through the same path, and gives a premultiplied square
    let px = art::load_square(&format!("{base}/ok.png"), 32).expect("square");
    assert_eq!(px.len(), 32 * 32 * 4);
    assert_eq!(&px[..4], &[10, 200, 30, 255]);

    // things that must not become files in the cache
    assert!(art::fetch(&format!("{base}/text.png")).is_none(), "not an image");
    assert!(art::fetch(&format!("{base}/missing.png")).is_none(), "404");
    assert!(art::fetch(&format!("{base}/big.png")).is_none(), "over the size cap");
    assert!(art::fetch(&format!("{base}/redirect-file")).is_none(), "a redirect to file:// is refused");
    assert_eq!(parts(), 0, "no half-written files left behind");

    // never even try for odd input
    for bad in ["ftp://example.com/a.png", "file:///no/such/file.png", "https://exa mple.com/a.png", "https://x/\n", "-o/tmp/x", "", "http://"] {
        assert!(art::fetch(bad).is_none(), "{bad:?}");
    }
    assert!(art::fetch(&format!("https://example.com/{}", "a".repeat(3000))).is_none(), "absurdly long");

    // the cache does not grow without bound
    let store = cache.join("notify-rs");
    for i in 0..300 {
        std::fs::write(store.join(format!("old{i:04}")), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    assert!(art::fetch(&format!("{base}/redirect-ok?again")).is_none(), "404 on that path, but pruning still ran");
    let left = std::fs::read_dir(&store).unwrap().count();
    assert!(left < 200, "pruned to about half, {left} left");
    let _ = std::fs::remove_dir_all(&dir);
}
