//! PNG and JPEG decoding against reference pixels (tools/gen_image_fixtures.py).
//! PNG must match exactly; JPEG must match libjpeg-turbo's own decode within
//! the stated tolerance.

use std::path::PathBuf;

use notify_rs::image;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images")
}

struct Case {
    name: String,
    kind: String,
    w: usize,
    h: usize,
    max_diff: u8,
    mean_diff: f64,
}

fn cases() -> Vec<Case> {
    let m = std::fs::read_to_string(dir().join("manifest.txt")).expect("manifest");
    m.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            Case { name: f[0].into(), kind: f[1].into(), w: f[2].parse().unwrap(), h: f[3].parse().unwrap(), max_diff: f[4].parse().unwrap(), mean_diff: f[5].parse().unwrap() }
        })
        .collect()
}

fn load(c: &Case) -> (Vec<u8>, Vec<u8>) {
    let ext = if c.kind == "jpeg" { "jpg" } else { "png" };
    (std::fs::read(dir().join(format!("{}.{ext}", c.name))).unwrap(), std::fs::read(dir().join(format!("{}.rgba", c.name))).unwrap())
}

#[test]
fn png_matches_exactly() {
    let mut n = 0;
    for c in cases().iter().filter(|c| c.kind == "exact") {
        let (data, want) = load(c);
        assert_eq!(image::dims(&data), Some((c.w as u32, c.h as u32)), "{}: dims", c.name);
        let img = image::decode(&data).unwrap_or_else(|e| panic!("{}: {e}", c.name));
        assert_eq!((img.w, img.h), (c.w, c.h), "{}", c.name);
        if img.rgba != want {
            let i = img.rgba.iter().zip(&want).position(|(a, b)| a != b).unwrap();
            panic!("{}: first mismatch at pixel ({}, {}) channel {}: got {:?} want {:?}", c.name, i / 4 % c.w, i / 4 / c.w, i % 4, &img.rgba[i / 4 * 4..i / 4 * 4 + 4], &want[i / 4 * 4..i / 4 * 4 + 4]);
        }
        n += 1;
    }
    assert!(n >= 35, "only {n} PNG fixtures");
}

#[test]
fn jpeg_matches_libjpeg() {
    let mut n = 0;
    let (mut worst, mut worst_mean) = (0u8, 0f64);
    for c in cases().iter().filter(|c| c.kind == "jpeg") {
        let (data, want) = load(c);
        assert_eq!(image::dims(&data), Some((c.w as u32, c.h as u32)), "{}: dims", c.name);
        let img = image::decode(&data).unwrap_or_else(|e| panic!("{}: {e}", c.name));
        assert_eq!((img.w, img.h), (c.w, c.h), "{}", c.name);
        assert_eq!(img.rgba.len(), want.len());
        let (mut max, mut sum) = (0u8, 0u64);
        for (a, b) in img.rgba.iter().zip(&want) {
            let d = a.abs_diff(*b);
            max = max.max(d);
            sum += d as u64;
        }
        let mean = sum as f64 / want.len() as f64;
        worst = worst.max(max);
        worst_mean = worst_mean.max(mean);
        assert!(max <= c.max_diff && mean <= c.mean_diff, "{}: max diff {max} (allowed {}), mean {mean:.3} (allowed {})", c.name, c.max_diff, c.mean_diff);
        n += 1;
    }
    eprintln!("{n} JPEGs; worst max diff {worst}, worst mean {worst_mean:.4}");
    assert!(n >= 40, "only {n} JPEG fixtures");
}

#[test]
fn truncated_jpeg_gives_a_partial_image_or_an_error() {
    for c in cases().iter().filter(|c| c.kind == "jpeg" && (c.name.contains("420_q75") || c.name.contains("444_prog"))) {
        let (data, _) = load(c);
        for frac in [0.2, 0.5, 0.8, 0.95] {
            let cut = (data.len() as f64 * frac) as usize;
            if let Ok(img) = image::decode(&data[..cut]) {
                assert_eq!((img.w, img.h), (c.w, c.h), "{} cut at {cut}", c.name);
            }
        }
    }
}

#[test]
fn corrupted_images_never_panic() {
    let mut s: u32 = 5;
    for c in cases() {
        let (data, _) = load(&c);
        for _ in 0..30 {
            let mut b = data.clone();
            for _ in 0..4 {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                let i = (s as usize >> 4) % b.len();
                b[i] ^= (s >> 20) as u8 | 1;
            }
            let _ = image::decode(&b);
            let cut = (s as usize >> 8) % b.len();
            let _ = image::decode(&b[..cut]);
        }
    }
}

#[test]
fn unknown_formats_are_errors() {
    assert!(image::decode(b"GIF89a....").is_err());
    assert!(image::decode(b"RIFF....WEBPVP8 ").is_err());
    assert!(image::decode(&[]).is_err());
    assert!(image::dims(b"nonsense").is_none());
}

#[test]
fn huge_declared_sizes_are_refused_before_allocating() {
    // IHDR claiming 100000 x 100000
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&(ihdr.len() as u32).to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&ihdr);
    png.extend_from_slice(&[0, 0, 0, 0]);
    assert!(image::decode(&png).is_err());
    // SOF0 claiming 65535 x 65535
    let jpg = [0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0xff, 0xff, 0xff, 0xff, 1, 1, 0x11, 0, 0xff, 0xd9];
    assert!(image::decode(&jpg).is_err());
}
