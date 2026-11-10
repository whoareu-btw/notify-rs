//! Variable CFF2 fonts (what NixOS ships as Noto Sans CJK) against fontTools:
//! outlines and advance widths at several weights, including a weight axis
//! whose default is the light end, one whose default is in the middle (negative
//! normalized coordinates), an `avar` bend, hints and `HVAR`.
//! The fonts and numbers come from tools/gen_cff2_fixtures.py.

use std::path::PathBuf;

use notify_rs::gfx::font::Font;
use notify_rs::gfx::raster::Cmd;

const REF: &str = include_str!("fixtures/fonts/cff2_ref.txt");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts").join(name)
}

struct Want {
    cp: u32,
    advance: f32,
    cmds: Vec<Cmd>,
}

/// font file, weight, glyphs
fn parse() -> Vec<(String, u16, Vec<Want>)> {
    let mut out: Vec<(String, u16, Vec<Want>)> = Vec::new();
    let mut font = String::new();
    for line in REF.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let n = |i: usize| -> f32 { f[i].parse().unwrap() };
        match f[0] {
            "font" => font = f[1].to_string(),
            "at" => out.push((font.clone(), f[1].parse().unwrap(), Vec::new())),
            "glyph" => out.last_mut().unwrap().2.push(Want { cp: f[1].parse().unwrap(), advance: n(2), cmds: Vec::new() }),
            "M" => out.last_mut().unwrap().2.last_mut().unwrap().cmds.push(Cmd::Move(n(1), n(2))),
            "L" => out.last_mut().unwrap().2.last_mut().unwrap().cmds.push(Cmd::Line(n(1), n(2))),
            "C" => out.last_mut().unwrap().2.last_mut().unwrap().cmds.push(Cmd::Cubic(n(1), n(2), n(3), n(4), n(5), n(6))),
            "Z" => out.last_mut().unwrap().2.last_mut().unwrap().cmds.push(Cmd::Close),
            other => panic!("bad reference line {other}"),
        }
    }
    out
}

fn same(a: &Cmd, b: &Cmd) -> bool {
    let near = |x: f32, y: f32| (x - y).abs() < 0.05;
    match (a, b) {
        (Cmd::Move(a, b), Cmd::Move(c, d)) | (Cmd::Line(a, b), Cmd::Line(c, d)) => near(*a, *c) && near(*b, *d),
        (Cmd::Cubic(a, b, c, d, e, f), Cmd::Cubic(g, h, i, j, k, l)) => [(a, g), (b, h), (c, i), (d, j), (e, k), (f, l)].iter().all(|(x, y)| near(**x, **y)),
        (Cmd::Close, Cmd::Close) => true,
        _ => false,
    }
}

#[test]
fn outlines_and_advances_match_fonttools_at_every_weight() {
    let mut checked = 0;
    for (file, weight, glyphs) in parse() {
        let font = Font::open_weight(&fixture(&file), 0, Some(weight)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(font.variable, "{file} is a variable font");
        assert_eq!(font.weight, weight.clamp(100, 900), "{file}: the weight it reports");
        for g in &glyphs {
            let ch = char::from_u32(g.cp).unwrap();
            let gid = font.glyph_index(ch);
            assert_ne!(gid, 0, "{file} {ch:?}: in the font");
            let at = format!("{file} @{weight} {ch:?}");
            assert!((font.advance(gid) - g.advance).abs() <= 0.5, "{at}: advance {} vs {} (HVAR)", font.advance(gid), g.advance);
            let got = font.outline(gid).unwrap_or_else(|| panic!("{at}: no outline")).cmds;
            assert_eq!(got.len(), g.cmds.len(), "{at}: number of path commands\n got {got:?}\nwant {:?}", g.cmds);
            for (i, (a, b)) in got.iter().zip(&g.cmds).enumerate() {
                assert!(same(a, b), "{at}: command {i}: {a:?} vs {b:?}");
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} glyphs were compared");
}

#[test]
fn weights_actually_change_the_outline() {
    // guards against a test that passes because every weight draws the same thing
    let f = |w| Font::open_weight(&fixture("vf_light.otf"), 0, Some(w)).unwrap();
    let (thin, bold) = (f(100), f(900));
    let gid = thin.glyph_index('I');
    let (tb, bb) = (thin.outline(gid).unwrap().bounds().unwrap(), bold.outline(gid).unwrap().bounds().unwrap());
    assert!((tb.2 - tb.0 - 60.0).abs() < 0.1, "the thin stem is 60 units: {tb:?}");
    assert!((bb.2 - bb.0 - 260.0).abs() < 0.1, "the bold stem is 260 units: {bb:?}");
    assert_eq!(thin.advance(gid), 260.0);
    assert_eq!(bold.advance(gid), 460.0);
}

#[test]
fn without_a_weight_the_default_instance_is_drawn() {
    let a = Font::open(&fixture("vf_light.otf"), 0).unwrap();
    let b = Font::open_weight(&fixture("vf_light.otf"), 0, Some(100)).unwrap();
    let gid = a.glyph_index('O');
    assert_eq!(a.outline(gid), b.outline(gid));
    assert_eq!(a.advance(gid), b.advance(gid));
    let c = Font::open(&fixture("vf_regular.otf"), 0).unwrap();
    let d = Font::open_weight(&fixture("vf_regular.otf"), 0, Some(400)).unwrap();
    let gid = c.glyph_index('O');
    assert_eq!(c.outline(gid), d.outline(gid));
}

#[test]
fn static_fonts_ignore_the_weight() {
    let p = std::path::Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
    if !p.exists() {
        return;
    }
    let a = Font::open(p, 0).unwrap();
    let b = Font::open_weight(p, 0, Some(700)).unwrap();
    assert_eq!((a.weight, b.weight), (400, 400));
    assert!(!b.variable);
    assert_eq!(a.outline(a.glyph_index('A')), b.outline(b.glyph_index('A')));
}

#[test]
fn damaged_variable_fonts_fail_cleanly() {
    let bytes = std::fs::read(fixture("vf_regular.otf")).unwrap();
    for cut in (0..bytes.len()).step_by(7) {
        if let Ok(font) = Font::from_bytes_weight(bytes[..cut].to_vec(), 0, Some(700)) {
            for gid in 0..12 {
                let _ = font.advance(gid);
                let _ = font.outline(gid);
            }
        }
    }
    // flipped bytes anywhere must not panic or hang either
    for i in (0..bytes.len()).step_by(3) {
        let mut b = bytes.clone();
        b[i] ^= 0xa5;
        if let Ok(font) = Font::from_bytes_weight(b, 0, Some(550)) {
            for gid in 0..12 {
                let _ = font.advance(gid);
                let _ = font.outline(gid);
            }
        }
    }
}
