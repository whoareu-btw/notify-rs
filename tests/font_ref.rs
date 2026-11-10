//! Glyph ids, advances, bounds and outline areas of real system fonts against
//! numbers produced by fontTools (see tools/gen_font_ref.py). Fonts that are
//! not installed are skipped.

use std::collections::HashMap;
use std::path::Path;

use notify_rs::gfx::font::Font;

const REF: &str = include_str!("fixtures/font_ref.txt");

#[test]
fn matches_fonttools() {
    let mut fonts: HashMap<(String, u32), Option<Font>> = HashMap::new();
    let (mut checked, mut worst_bounds, mut worst_area) = (0usize, 0.0f32, 0.0f32);
    for line in REF.lines() {
        let f: Vec<&str> = line.split('|').collect();
        let (path, face, cp): (&str, u32, u32) = (f[0], f[1].parse().unwrap(), f[2].parse().unwrap());
        let (gid, adv): (u32, f32) = (f[3].parse().unwrap(), f[4].parse().unwrap());
        let want_b: Vec<f32> = f[5..9].iter().map(|v| v.parse().unwrap()).collect();
        let want_area: f32 = f[9].parse().unwrap();

        let font = fonts
            .entry((path.to_string(), face))
            .or_insert_with(|| if Path::new(path).exists() { Some(Font::open(Path::new(path), face).unwrap_or_else(|e| panic!("{path}: {e}"))) } else { None });
        let Some(font) = font else { continue };

        let ch = char::from_u32(cp).unwrap();
        assert_eq!(font.glyph_index(ch), gid, "{path} {ch:?}: glyph id");
        assert_eq!(font.advance(gid), adv, "{path} {ch:?}: advance");

        let p = font.outline(gid).unwrap_or_else(|| panic!("{path} {ch:?}: no outline"));
        match p.bounds() {
            Some((x0, y0, x1, y1)) => {
                let d = (x0 - want_b[0]).abs().max((y0 - want_b[1]).abs()).max((x1 - want_b[2]).abs()).max((y1 - want_b[3]).abs());
                worst_bounds = worst_bounds.max(d);
                assert!(d < 1.0, "{path} {ch:?}: bounds {:?} vs {want_b:?}", (x0, y0, x1, y1));
            }
            None => assert!(want_b.iter().all(|v| *v == 0.0), "{path} {ch:?}: empty outline but reference has bounds {want_b:?}"),
        }
        let area = p.signed_area();
        let tol = (want_area.abs() * 0.002).max(5.0);
        worst_area = worst_area.max((area - want_area).abs() / want_area.abs().max(1.0));
        assert!((area - want_area).abs() <= tol, "{path} {ch:?}: area {area} vs {want_area}");
        checked += 1;
    }
    eprintln!("checked {checked} glyphs; worst bounds error {worst_bounds}, worst relative area error {worst_area}");
    let present = fonts.values().filter(|f| f.is_some()).count();
    if present == 0 {
        eprintln!("skip: none of the reference fonts are installed");
    } else {
        assert!(checked > 20, "only {checked} glyphs were checked");
    }
}

#[test]
fn font_metrics_and_names_are_sane() {
    for (path, family) in [
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "DejaVu Sans"),
        ("/usr/share/fonts/opentype/inter/Inter-Regular.otf", "Inter"),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", "DejaVu Sans"),
    ] {
        if !Path::new(path).exists() {
            continue;
        }
        let f = Font::open(Path::new(path), 0).unwrap();
        assert_eq!(f.family, family, "{path}");
        assert!(f.ascent > 0.0 && f.descent < 0.0 && f.units_per_em >= 1000.0);
    }
    if Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf").exists() {
        let b = Font::open(Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"), 0).unwrap();
        assert!(b.weight >= 700, "{}", b.weight);
        assert!(b.style.to_lowercase().contains("bold"), "{}", b.style);
    }
}

#[test]
fn collections_expose_every_face() {
    let p = "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc";
    if !Path::new(p).exists() {
        return;
    }
    let mut seen = Vec::new();
    for i in 0..8 {
        match Font::open(Path::new(p), i) {
            Ok(f) => seen.push(f.family),
            Err(_) => break,
        }
    }
    assert!(seen.len() >= 4, "{seen:?}");
    assert!(seen[0].contains("JP"), "face 0 should be the Japanese one: {seen:?}");
}

#[test]
fn broken_fonts_are_errors_not_panics() {
    assert!(Font::from_bytes(vec![], 0).is_err());
    assert!(Font::from_bytes(b"not a font at all".to_vec(), 0).is_err());
    // truncate and corrupt a real font at many places
    let p = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
    let Ok(bytes) = std::fs::read(p) else { return };
    for cut in [4usize, 12, 100, 500, 5000, 100_000] {
        if let Ok(f) = Font::from_bytes(bytes[..cut.min(bytes.len())].to_vec(), 0) {
            for g in 0..50 {
                let _ = f.outline(g);
            }
        }
    }
    let mut s: u32 = 99;
    for _ in 0..40 {
        let mut b = bytes.clone();
        for _ in 0..30 {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = (s as usize) % b.len().min(200_000);
            b[i] ^= (s >> 16) as u8;
        }
        if let Ok(f) = Font::from_bytes(b, 0) {
            for ch in "AgB日é".chars() {
                let g = f.glyph_index(ch);
                let _ = f.advance(g);
                let _ = f.outline(g);
            }
        }
    }
}
