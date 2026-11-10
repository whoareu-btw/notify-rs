//! Minimal PNG writer (stored deflate blocks, no compression). Used for
//! `notify-rs preview` and for looking at rendering results while developing.

use super::canvas::{Canvas, Color};

fn crc32(data: &[u8], mut crc: u32) -> u32 {
    // bitwise: only used on debug screenshots
    crc = !crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let c = crc32(data, crc32(kind, 0));
    out.extend_from_slice(&c.to_be_bytes());
}

/// Encode straight RGBA8 pixels as a PNG.
pub fn encode_rgba(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for row in rgba.chunks_exact(w * 4).take(h) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bit, RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The canvas as a PNG, composited over `bg` (premultiplied pixels are converted back).
pub fn canvas_png(cv: &Canvas, bg: Option<Color>) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(cv.w * cv.h * 4);
    for &p in cv.pixels() {
        let (a, r, g, b) = (p >> 24, (p >> 16) & 255, (p >> 8) & 255, p & 255);
        match bg {
            Some(bg) => {
                // premultiplied over an opaque background
                let ia = 255 - a;
                let ch = |s: u32, d: u32| (s + (d * ia + 127) / 255).min(255) as u8;
                rgba.extend_from_slice(&[ch(r, bg.r()), ch(g, bg.g()), ch(b, bg.b()), 255]);
            }
            None => {
                let un = |v: u32| if a == 0 { 0 } else { ((v * 255 + a / 2) / a).min(255) as u8 };
                rgba.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
            }
        }
    }
    encode_rgba(cv.w, cv.h, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_checksums() {
        assert_eq!(crc32(b"123456789", 0), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn png_structure() {
        let p = encode_rgba(2, 1, &[255, 0, 0, 255, 0, 255, 0, 128]);
        assert_eq!(&p[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&p[12..16], b"IHDR");
        assert_eq!(&p[p.len() - 8..p.len() - 4], b"IEND");
    }

    #[test]
    fn python_pil_can_read_it() {
        // cross-check with an independent decoder when python+PIL exist
        let mut cv = Canvas::new(3, 2);
        cv.clear(Color::rgb(10, 20, 30));
        cv.fill_rect(super::super::canvas::Rect::new(1, 0, 1, 1), Color::rgb(200, 100, 50));
        let png = canvas_png(&cv, None);
        let dir = std::env::temp_dir().join(format!("notify-rs-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.png");
        std::fs::write(&f, png).unwrap();
        let out = std::process::Command::new("python3")
            .args(["-I", "-c", "import sys\nfrom PIL import Image\nim=Image.open(sys.argv[1]).convert('RGBA')\nprint(im.size, im.getpixel((0,0)), im.getpixel((1,0)))", f.to_str().unwrap()])
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        if let Ok(o) = out {
            if o.status.success() {
                let t = String::from_utf8_lossy(&o.stdout);
                assert!(t.contains("(3, 2) (10, 20, 30, 255) (200, 100, 50, 255)"), "{t}");
            }
        }
    }
}
