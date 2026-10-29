//! Text: font fallback chains, layout, wrapping and drawing with a glyph cache.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::rc::Rc;

use super::canvas::{Canvas, Color};
use super::font::Font;
use super::fontdb;
use super::raster::fill;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    Regular,
    Bold,
}

impl Style {
    fn idx(self) -> usize {
        self as usize
    }
}

/// A rasterised glyph: `a` is `w` x `h` coverage, `left`/`top` position it
/// relative to the pen (top is measured up from the baseline).
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub w: usize,
    pub h: usize,
    pub a: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// index into `Fonts::faces`; `usize::MAX` for a missing glyph
    pub font: usize,
    pub gid: u32,
    /// pen position in pixels
    pub x: f32,
}

const NONE: (usize, u32) = (usize::MAX, 0);
const CACHE_LIMIT: usize = 4000;

pub struct Fonts {
    faces: Vec<Rc<Font>>,
    chains: [Vec<usize>; 2],
    synth_bold: bool,
    pick_cache: HashMap<(Style, char), (usize, u32)>,
    glyph_cache: HashMap<(usize, u32, u32, bool), Option<Rc<Bitmap>>>,
    gamma: [u8; 256],
}

fn zero_width(c: char) -> bool {
    matches!(c as u32, 0x200b..=0x200f | 0x202a..=0x202e | 0x2060 | 0x2066..=0x2069 | 0xfe00..=0xfe0f | 0xfeff)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2e80..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0xff00..=0xffef | 0x20000..=0x3ffff)
}

/// Characters that must not start a line (closing punctuation).
fn no_start(c: char) -> bool {
    "、。，．・：；！？）］｝〕〉》」』】゛゜ー々ゝゞヽヾ,.;:!?)]}%".contains(c)
}

/// Characters that must not end a line (opening brackets).
fn no_end(c: char) -> bool {
    "（［｛〔〈《「『【([{".contains(c)
}

fn can_break(prev: char, next: char) -> bool {
    if next == ' ' || no_start(next) || no_end(prev) {
        return false;
    }
    if prev == ' ' {
        return true;
    }
    if is_cjk(prev) || is_cjk(next) {
        return true;
    }
    (prev == '-' || prev == '/') && next.is_alphanumeric()
}

impl Fonts {
    /// Build from already opened fonts. `bold` may be empty: the regular
    /// chain is then emboldened artificially.
    pub fn from_fonts(regular: Vec<Font>, bold: Vec<Font>) -> Fonts {
        let mut faces: Vec<Rc<Font>> = Vec::new();
        let mut chain = |list: Vec<Font>| -> Vec<usize> {
            list.into_iter()
                .map(|f| {
                    faces.push(Rc::new(f));
                    faces.len() - 1
                })
                .collect()
        };
        let reg = chain(regular);
        let synth = bold.is_empty();
        let bld = if synth { reg.clone() } else { chain(bold) };
        let mut f = Fonts {
            faces,
            chains: [reg, bld],
            synth_bold: synth,
            pick_cache: HashMap::new(),
            glyph_cache: HashMap::new(),
            gamma: [0; 256],
        };
        f.set_gamma(0.8);
        f
    }

    /// Lower exponents make partly covered pixels (thin strokes) stronger.
    pub fn set_gamma(&mut self, g: f32) {
        for (i, v) in self.gamma.iter_mut().enumerate() {
            *v = (255.0 * (i as f32 / 255.0).powf(g) + 0.5) as u8;
        }
        self.glyph_cache.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.chains[0].is_empty()
    }

    /// Find fonts on this system. The UI font is JetBrainsMono Nerd Font Propo
    /// and Japanese/Chinese/Korean text comes from Noto Sans CJK; anything
    /// missing from those two falls back to whatever fontconfig calls
    /// `sans-serif`. `NOTIFY_RS_FONT` (`file[#index]:file...`) goes first when set.
    /// Without fontconfig the usual font directories are scanned instead.
    pub fn system() -> Result<Fonts, String> {
        Fonts::system_with_report().map(|(f, _)| f)
    }

    /// Like `system`, plus one line per font that was loaded or rejected
    /// (what `notify-rs fonts` prints).
    pub fn system_with_report() -> Result<(Fonts, Vec<String>), String> {
        /// First choice for Latin text and the icon glyphs.
        const UI_FAMILIES: [&str; 3] = ["JetBrainsMono Nerd Font Propo", "JetBrainsMono NFP", "JetBrainsMono Nerd Font"];
        /// Japanese first; every one of these also covers Chinese and Korean.
        const CJK_FAMILIES: [&str; 5] = ["Noto Sans CJK JP", "Noto Sans JP", "Noto Sans CJK SC", "Noto Sans CJK", "Source Han Sans"];

        let mut report: Vec<String> = Vec::new();
        let scanned: std::cell::OnceCell<Vec<super::font::FaceInfo>> = std::cell::OnceCell::new();

        // the best installed face of the first family that exists, at this weight
        let preferred = |families: &[&str], weight: u16| -> Option<(PathBuf, u32)> {
            for fam in families {
                if let Ok(Some(found)) = fontdb::fc_family(fam, weight >= 600) {
                    return Some(found);
                }
            }
            // fontconfig does not know it (or is not there): look at the files
            let faces = scanned.get_or_init(|| fontdb::scan(&fontdb::font_dirs()));
            families.iter().find_map(|fam| fontdb::find(faces, fam, weight).map(|f| (f.path.clone(), f.index)))
        };

        let mut explicit: Vec<(PathBuf, u32)> = Vec::new();
        if let Some(v) = std::env::var_os("NOTIFY_RS_FONT") {
            for part in v.to_string_lossy().split(':').filter(|s| !s.is_empty()) {
                let (p, i) = match part.rsplit_once('#') {
                    Some((p, i)) => (p, i.parse().unwrap_or(0)),
                    None => (part, 0),
                };
                explicit.push((PathBuf::from(p), i));
            }
        }

        // what the system itself would use, per weight: (Latin, Japanese)
        let system_sans = |pattern: &str| (fontdb::fc_match(pattern, 3), fontdb::fc_match(&format!("{pattern}:lang=ja"), 4));
        let (sans_reg, ja_reg) = system_sans("sans-serif");
        let (sans_bold, ja_bold) = system_sans("sans-serif:bold");
        // without fontconfig, well-known families found by scanning take its place
        let mut fb_latin: [Vec<(PathBuf, u32)>; 2] = [Vec::new(), Vec::new()];
        let mut fb_cjk: [Vec<(PathBuf, u32)>; 2] = [Vec::new(), Vec::new()];
        if sans_reg.is_empty() && ja_reg.is_empty() {
            let faces = scanned.get_or_init(|| fontdb::scan(&fontdb::font_dirs()));
            const LATIN: [&str; 7] = ["Inter", "Noto Sans", "Cantarell", "Roboto", "Ubuntu", "DejaVu Sans", "Liberation Sans"];
            const CJK: [&str; 7] = ["IPAexGothic", "IPAGothic", "Takao", "VL Gothic", "WenQuanYi", "Unifont", "Droid Sans Fallback"];
            for (i, weight) in [400u16, 700].into_iter().enumerate() {
                if let Some(f) = LATIN.iter().find_map(|fam| fontdb::find(faces, fam, weight)) {
                    fb_latin[i].push((f.path.clone(), f.index));
                }
                fb_cjk[i].extend(CJK.iter().filter_map(|fam| fontdb::find(faces, fam, weight)).map(|f| (f.path.clone(), f.index)));
                // DejaVu has the symbols and arrows the others lack
                fb_cjk[i].extend(fontdb::find(faces, "DejaVu Sans", weight).map(|f| (f.path.clone(), f.index)));
            }
        }

        let mut chains: [Vec<(PathBuf, u32)>; 2] = [explicit, Vec::new()];
        for (i, weight) in [400u16, 700].into_iter().enumerate() {
            let (sans, ja) = if i == 0 { (&sans_reg, &ja_reg) } else { (&sans_bold, &ja_bold) };
            let latin: Vec<(PathBuf, u32)> = sans.iter().chain(&fb_latin[i]).cloned().collect();
            let rest: Vec<(PathBuf, u32)> = ja.iter().chain(&fb_cjk[i]).cloned().collect();
            let ordered = order_chain(preferred(&UI_FAMILIES, weight), latin, preferred(&CJK_FAMILIES, weight), rest);
            chains[i].extend(ordered);
        }
        let [reg, bold] = chains;

        let mut open = |list: &[(PathBuf, u32)], weight: u16, role: &str| -> Vec<Font> {
            let mut seen: Vec<&(PathBuf, u32)> = Vec::new();
            let mut out = Vec::new();
            for e in list {
                if seen.contains(&e) {
                    continue;
                }
                seen.push(e);
                match Font::open_weight(&e.0, e.1, Some(weight)) {
                    Ok(f) => {
                        let var = if f.variable { format!(", variable font set to weight {}", f.weight) } else { String::new() };
                        report.push(format!("{role:<8} {} {}  ({}#{}{var})", f.family, f.style, e.0.display(), e.1));
                        out.push(f);
                    }
                    Err(err) => report.push(format!("skipped  {}#{}: {err}", e.0.display(), e.1)),
                }
            }
            out
        };

        let regular = open(&reg, 400, "regular");
        if regular.is_empty() {
            return Err("no usable font found. Install one (e.g. JetBrainsMono Nerd Font, DejaVu Sans or Noto Sans) or point NOTIFY_RS_FONT at a .ttf/.otf file".into());
        }
        let mut bolds = open(&bold, 700, "bold");
        // fc-match returns the regular face when there is no bold: then fake it
        let truly_bold = bolds.first().map(|f| f.weight >= 600).unwrap_or(false);
        if !truly_bold {
            bolds.clear();
            report.push("bold     no bold face found: the regular face is thickened instead".into());
        }
        Ok((Fonts::from_fonts(regular, bolds), report))
    }

    /// Which face draws `ch` ("Family Style"), or None if no font has it.
    pub fn which(&mut self, style: Style, ch: char) -> Option<String> {
        let (fi, _) = self.pick(style, ch);
        (fi != usize::MAX).then(|| format!("{} {}", self.faces[fi].family, self.faces[fi].style))
    }

    pub fn describe(&self) -> Vec<String> {
        self.chains[0].iter().map(|&i| format!("{} {}", self.faces[i].family, self.faces[i].style)).collect()
    }

    fn pick(&mut self, style: Style, ch: char) -> (usize, u32) {
        if let Some(&r) = self.pick_cache.get(&(style, ch)) {
            return r;
        }
        let mut found = NONE;
        for &fi in &self.chains[style.idx()] {
            let g = self.faces[fi].glyph_index(ch);
            if g != 0 {
                found = (fi, g);
                break;
            }
        }
        // a bold chain without the glyph falls back to the regular chain
        if found == NONE && style == Style::Bold && !self.synth_bold {
            for &fi in &self.chains[0] {
                let g = self.faces[fi].glyph_index(ch);
                if g != 0 {
                    found = (fi, g);
                    break;
                }
            }
        }
        self.pick_cache.insert((style, ch), found);
        found
    }

    /// (ascent, descent, line height) in pixels for the primary font; descent is positive.
    pub fn metrics(&self, style: Style, px: f32) -> (f32, f32, f32) {
        let Some(&fi) = self.chains[style.idx()].first() else { return (px, px * 0.25, px * 1.25) };
        let f = &self.faces[fi];
        let s = px / f.units_per_em;
        (f.ascent * s, -f.descent * s, (f.ascent - f.descent + f.line_gap) * s)
    }

    pub fn layout(&mut self, style: Style, text: &str, px: f32) -> (Vec<Glyph>, f32) {
        let mut out = Vec::with_capacity(text.len());
        let mut x = 0.0f32;
        for ch in text.chars() {
            if zero_width(ch) {
                continue;
            }
            let ch = if ch == '\t' { ' ' } else { ch };
            if ch.is_control() {
                continue;
            }
            let (fi, gid) = self.pick(style, ch);
            if fi == usize::MAX {
                out.push(Glyph { font: usize::MAX, gid: 0, x });
                x += px * 0.6;
                continue;
            }
            out.push(Glyph { font: fi, gid, x });
            let f = &self.faces[fi];
            x += f.advance(gid) * px / f.units_per_em;
        }
        (out, x)
    }

    pub fn measure(&mut self, style: Style, text: &str, px: f32) -> f32 {
        self.layout(style, text, px).1
    }

    fn bitmap(&mut self, fi: usize, gid: u32, px: f32, bold: bool) -> Option<Rc<Bitmap>> {
        let key = (fi, gid, (px * 64.0) as u32, bold);
        if let Some(b) = self.glyph_cache.get(&key) {
            return b.clone();
        }
        if self.glyph_cache.len() > CACHE_LIMIT {
            self.glyph_cache.clear();
        }
        let bmp = self.render(fi, gid, px, bold).map(Rc::new);
        self.glyph_cache.insert(key, bmp.clone());
        bmp
    }

    fn render(&self, fi: usize, gid: u32, px: f32, bold: bool) -> Option<Bitmap> {
        let f = &self.faces[fi];
        let path = f.outline(gid)?;
        let s = px / f.units_per_em;
        let (x0, y0, x1, y1) = path.bounds()?;
        // pixel box, y flipped (font units go up, pixels go down)
        let pad = if bold { 2 } else { 1 };
        let left = (x0 * s).floor() as i32 - pad;
        let top = (y1 * s).ceil() as i32 + pad; // above the baseline
        let right = (x1 * s).ceil() as i32 + pad + if bold { 1 } else { 0 };
        let bottom = (-y0 * s).ceil() as i32 + pad; // below the baseline (as y-down)
        let w = (right - left).max(1) as usize;
        let h = (top + bottom).max(1) as usize;
        if w > 1024 || h > 1024 {
            return None;
        }
        let mut moved = super::raster::Path::new();
        moved.append_transformed(&path, [s, 0.0, 0.0, -s, -left as f32, top as f32]);
        let mut cov = fill(&moved, w, h).a;

        if bold {
            // smear to the right by ~px/18 to fake a heavier weight
            let e = (px / 18.0).clamp(0.3, 1.0);
            for row in cov.chunks_exact_mut(w) {
                let mut prev = 0u8;
                for v in row.iter_mut() {
                    let cur = *v;
                    *v = cur.max((prev as f32 * e) as u8);
                    prev = cur;
                }
            }
        }
        for v in &mut cov {
            *v = self.gamma[*v as usize];
        }
        Some(Bitmap { left, top, w, h, a: cov })
    }

    /// Draw `text` with its baseline at `baseline`, starting at `x`. Returns the width.
    pub fn draw(&mut self, cv: &mut Canvas, style: Style, x: f32, baseline: f32, text: &str, px: f32, color: Color) -> f32 {
        let (glyphs, width) = self.layout(style, text, px);
        let by = baseline.round() as i32;
        for g in glyphs {
            if g.font == usize::MAX {
                continue;
            }
            // faces without a real bold (CJK fonts often have only Regular) are emboldened artificially
            let bold = style == Style::Bold && (self.synth_bold || self.faces[g.font].weight < 600);
            let Some(b) = self.bitmap(g.font, g.gid, px, bold) else { continue };
            let gx = (x + g.x).round() as i32;
            cv.blit_a8(gx + b.left, by - b.top, b.w, b.h, &b.a, color);
        }
        width
    }

    /// Shorten `text` with an ellipsis so it fits in `max_w`.
    pub fn ellipsize(&mut self, style: Style, text: &str, px: f32, max_w: f32) -> String {
        if self.measure(style, text, px) <= max_w {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
            if self.measure(style, &cand, px) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let head: String = chars[..lo].iter().collect();
        head.trim_end().to_string() + "…"
    }

    /// Break `text` into lines no wider than `max_w`. Newlines are kept as
    /// breaks; CJK text may break between any two characters. If more than
    /// `max_lines` result, the last kept line ends with an ellipsis.
    pub fn wrap(&mut self, style: Style, text: &str, px: f32, max_w: f32, max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for para in text.split('\n') {
            let chars: Vec<char> = para
                .chars()
                .filter(|c| !zero_width(*c))
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .collect();
            if chars.is_empty() {
                lines.push(String::new());
                continue;
            }
            let widths: Vec<f32> = chars.iter().map(|&c| self.measure(style, &c.to_string(), px)).collect();
            let mut start = 0;
            while start < chars.len() {
                let (mut w, mut end, mut last_break) = (0.0f32, start, None);
                while end < chars.len() {
                    let cw = widths[end];
                    // a space at the end of a line may hang over the edge
                    if w + cw > max_w && end > start && chars[end] != ' ' {
                        break;
                    }
                    w += cw;
                    end += 1;
                    if end < chars.len() && can_break(chars[end - 1], chars[end]) {
                        last_break = Some(end);
                    }
                }
                let cut = if end == chars.len() { end } else { last_break.filter(|&b| b > start).unwrap_or(end) };
                let line: String = chars[start..cut].iter().collect();
                lines.push(line.trim_end().to_string());
                start = cut;
                while start < chars.len() && chars[start] == ' ' {
                    start += 1;
                }
            }
        }
        if lines.len() > max_lines && max_lines > 0 {
            lines.truncate(max_lines);
            let last = lines.pop().unwrap_or_default();
            let marked = format!("{last}…");
            lines.push(self.ellipsize(style, &marked, px, max_w));
        }
        lines
    }
}

/// Order the candidate files of one weight. The UI font (JetBrainsMono Nerd
/// Font Propo) goes first, Noto CJK draws the Japanese/Chinese/Korean text, and
/// the system's own sans fonts catch whatever is left. When there is no UI font
/// the Latin system font moves ahead of Noto CJK, so plain English text is not
/// silently set in a CJK face.
fn order_chain(ui: Option<Entry>, latin: Vec<Entry>, cjk: Option<Entry>, rest: Vec<Entry>) -> Vec<Entry> {
    let mut out = Vec::new();
    let have_ui = ui.is_some();
    out.extend(ui);
    if !have_ui {
        out.extend(latin.iter().cloned());
    }
    out.extend(cjk);
    if have_ui {
        out.extend(latin);
    }
    out.extend(rest);
    out
}

type Entry = (PathBuf, u32);

/// Convenience for tests and tools: fonts from explicit files.
pub fn fonts_from_files(regular: &[&FsPath], bold: &[&FsPath]) -> Result<Fonts, String> {
    let open = |l: &[&FsPath]| l.iter().map(|p| Font::open(p, 0)).collect::<Result<Vec<_>, _>>();
    Ok(Fonts::from_fonts(open(regular)?, open(bold)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dejavu() -> Option<Fonts> {
        let r = FsPath::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        let b = FsPath::new("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf");
        if !r.exists() || !b.exists() {
            return None;
        }
        fonts_from_files(&[r], &[b]).ok()
    }

    #[test]
    fn chain_order_puts_the_ui_font_first_and_never_latin_in_cjk() {
        let e = |n: &str| (PathBuf::from(n), 0u32);
        // JetBrains installed: JetBrains, Noto CJK, system sans, the rest
        let c = order_chain(Some(e("jb")), vec![e("inter")], Some(e("cjk")), vec![e("ja"), e("dejavu")]);
        assert_eq!(c, [e("jb"), e("cjk"), e("inter"), e("ja"), e("dejavu")]);
        // JetBrains missing: the Latin system font must come before Noto CJK
        let c = order_chain(None, vec![e("inter")], Some(e("cjk")), vec![e("ja")]);
        assert_eq!(c, [e("inter"), e("cjk"), e("ja")]);
        // nothing but CJK installed: it still works
        assert_eq!(order_chain(None, vec![], Some(e("cjk")), vec![]), [e("cjk")]);
        assert!(order_chain(None, vec![], None, vec![]).is_empty());
    }

    #[test]
    fn break_rules() {
        assert!(can_break(' ', 'a'));
        assert!(!can_break('a', 'b'));
        assert!(can_break('日', '本'));
        assert!(!can_break('日', '。'), "punctuation does not start a line");
        assert!(!can_break('「', '日'), "opening bracket does not end a line");
        assert!(can_break('a', '日'));
        assert!(can_break('-', 'x'));
        assert!(!can_break('a', ' '));
    }

    #[test]
    fn measures_and_scales_linearly() {
        let Some(mut f) = dejavu() else { return };
        let w1 = f.measure(Style::Regular, "Hello world", 10.0);
        let w2 = f.measure(Style::Regular, "Hello world", 20.0);
        assert!(w1 > 40.0 && w1 < 80.0, "{w1}");
        assert!((w2 / w1 - 2.0).abs() < 1e-4);
        assert_eq!(f.measure(Style::Regular, "", 10.0), 0.0);
        let bold = f.measure(Style::Bold, "Hello world", 10.0);
        assert!(bold > w1, "the bold face is wider");
        assert_eq!(f.measure(Style::Regular, "a\u{200b}b", 10.0), f.measure(Style::Regular, "ab", 10.0), "zero-width chars add nothing");
    }

    #[test]
    fn draws_visible_pixels_in_the_right_place() {
        let Some(mut f) = dejavu() else { return };
        let mut cv = Canvas::new(80, 30);
        let w = f.draw(&mut cv, Style::Regular, 4.0, 22.0, "Hi", 20.0, Color::rgb(255, 255, 255));
        assert!(w > 15.0);
        let lit: Vec<(usize, usize)> =
            (0..30).flat_map(|y| (0..80).map(move |x| (x, y))).filter(|&(x, y)| cv.pixel(x, y) != 0).collect();
        assert!(lit.len() > 40);
        // ink sits on the baseline: nothing below it, something just above
        assert!(lit.iter().all(|&(_, y)| y <= 23), "no ink far below the baseline");
        assert!(lit.iter().any(|&(_, y)| y >= 20));
        assert!(lit.iter().all(|&(x, _)| x >= 3 && (x as f32) < 4.0 + w + 2.0));
    }

    #[test]
    fn bold_is_visibly_heavier_even_when_synthetic() {
        let r = FsPath::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        if !r.exists() {
            return;
        }
        let mut synth = fonts_from_files(&[r], &[]).unwrap();
        let ink = |f: &mut Fonts, s: Style| {
            let mut cv = Canvas::new(60, 24);
            f.draw(&mut cv, s, 2.0, 18.0, "lI", 18.0, Color::rgb(255, 255, 255));
            cv.pixels().iter().map(|p| (p >> 24) as u32).sum::<u32>()
        };
        let (a, b) = (ink(&mut synth, Style::Regular), ink(&mut synth, Style::Bold));
        assert!(b as f32 > a as f32 * 1.15, "{a} vs {b}");
    }

    #[test]
    fn wraps_words_and_respects_newlines() {
        let Some(mut f) = dejavu() else { return };
        let one = f.measure(Style::Regular, "gamma delta", 14.0).max(f.measure(Style::Regular, "alpha beta", 14.0));
        let lines = f.wrap(Style::Regular, "alpha beta gamma delta", 14.0, one + 2.0, 10);
        assert_eq!(lines, vec!["alpha beta", "gamma delta"]);
        let lines = f.wrap(Style::Regular, "one\n\ntwo", 14.0, 500.0, 10);
        assert_eq!(lines, vec!["one", "", "two"]);
        for l in f.wrap(Style::Regular, "a long line with several words in it", 14.0, 80.0, 10) {
            assert!(f.measure(Style::Regular, &l, 14.0) <= 80.5, "{l}");
        }
    }

    #[test]
    fn long_words_are_broken_instead_of_overflowing() {
        let Some(mut f) = dejavu() else { return };
        let url = "https://example.com/a/very/long/path/that/never/ends/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let lines = f.wrap(Style::Regular, url, 14.0, 120.0, 20);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(f.measure(Style::Regular, l, 14.0) <= 120.5, "{l}");
        }
        assert_eq!(lines.concat(), url, "nothing lost or duplicated");
    }

    #[test]
    fn truncates_with_an_ellipsis() {
        let Some(mut f) = dejavu() else { return };
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let lines = f.wrap(Style::Regular, text, 14.0, 100.0, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        assert!(f.measure(Style::Regular, &lines[1], 14.0) <= 100.5);
        let e = f.ellipsize(Style::Regular, text, 14.0, 90.0);
        assert!(e.ends_with('…') && f.measure(Style::Regular, &e, 14.0) <= 90.5);
        assert_eq!(f.ellipsize(Style::Regular, "short", 14.0, 500.0), "short");
        assert_eq!(f.ellipsize(Style::Regular, "anything", 14.0, 0.0), "…");
    }

    #[test]
    fn cjk_falls_back_and_wraps_anywhere() {
        let cjk = FsPath::new("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc");
        let r = FsPath::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        if !cjk.exists() || !r.exists() {
            return;
        }
        let mut f = fonts_from_files(&[r, cjk], &[]).unwrap();
        // DejaVu has no kanji: the width must come from the CJK font (about 1em per char)
        let w = f.measure(Style::Regular, "夜に駆ける", 20.0);
        assert!(w > 80.0 && w < 110.0, "{w}");
        let mut cv = Canvas::new(120, 30);
        f.draw(&mut cv, Style::Regular, 2.0, 22.0, "夜に駆ける", 20.0, Color::rgb(255, 255, 255));
        assert!(cv.pixels().iter().filter(|&&p| p != 0).count() > 300, "kanji were drawn");
        let lines = f.wrap(Style::Regular, "夜に駆ける、さあ。", 20.0, 60.0, 5);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|l| !l.starts_with('。') && !l.starts_with('、')), "{lines:?}");
        assert_eq!(lines.concat(), "夜に駆ける、さあ。");
    }

    #[test]
    fn missing_glyphs_take_space_but_draw_nothing() {
        let Some(mut f) = dejavu() else { return };
        let (g, w) = f.layout(Style::Regular, "a\u{10ffff}b", 10.0);
        assert_eq!(g.len(), 3);
        assert_eq!(g[1].font, usize::MAX);
        assert!(w > 10.0);
    }

    #[test]
    fn metrics_are_positive() {
        let Some(f) = dejavu() else { return };
        let (a, d, h) = f.metrics(Style::Regular, 16.0);
        assert!(a > 10.0 && d > 2.0 && h >= a + d);
    }
}
