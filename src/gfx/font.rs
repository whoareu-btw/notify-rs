//! TrueType / OpenType font files: tables, character map, metrics and outlines
//! (`glyf`, CFF and CFF2). Handles collections (`.ttc`) and, for CFF2 variable
//! fonts, picking a weight. Fonts are memory mapped, so
//! only the pages that are actually used take memory.

use std::path::Path as FsPath;
use std::sync::Arc;

use super::cff::Cff;
use super::raster::Path;
use super::var::{self, Hvar};
use crate::sys::Map;

/// The bytes of a font file.
pub enum FontData {
    Mapped(Map),
    Owned(Vec<u8>),
}

impl FontData {
    pub fn bytes(&self) -> &[u8] {
        match self {
            FontData::Mapped(m) => m.as_slice(),
            FontData::Owned(v) => v,
        }
    }
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
}
fn i16_at(d: &[u8], o: usize) -> Option<i16> {
    u16_at(d, o).map(|v| v as i16)
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*d.get(o)?, *d.get(o + 1)?, *d.get(o + 2)?, *d.get(o + 3)?]))
}

type Range = (usize, usize);

#[derive(Clone, Copy, Debug)]
struct Cmap {
    offset: usize,
    format: u16,
}

enum Outlines {
    Glyf { loca: Range, glyf: Range, long_loca: bool },
    Cff { table: Range, cff: Cff },
}

pub struct Font {
    data: Arc<FontData>,
    pub units_per_em: f32,
    pub num_glyphs: u32,
    pub ascent: f32,
    /// negative below the baseline
    pub descent: f32,
    pub line_gap: f32,
    hmtx: Range,
    num_hmetrics: usize,
    cmap: Option<Cmap>,
    outlines: Outlines,
    pub family: String,
    pub style: String,
    pub weight: u16,
    pub italic: bool,
    /// a variable font whose weight axis was set (CFF2) or could be set
    pub variable: bool,
    hvar: Option<Hvar>,
    /// HVAR region scalars at the chosen weight
    hvar_scalars: Vec<f32>,
}

/// Names of the tables in one face.
fn read_tables(d: &[u8], face_off: usize) -> Option<Vec<([u8; 4], Range)>> {
    let n = u16_at(d, face_off + 4)? as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let r = face_off + 12 + i * 16;
        let tag = [*d.get(r)?, *d.get(r + 1)?, *d.get(r + 2)?, *d.get(r + 3)?];
        let off = u32_at(d, r + 8)? as usize;
        let len = u32_at(d, r + 12)? as usize;
        if off.checked_add(len)? > d.len() {
            continue; // damaged table: ignore it, others may still work
        }
        out.push((tag, (off, len)));
    }
    Some(out)
}

/// How many faces a file holds (1 unless it is a collection).
pub fn face_count(d: &[u8]) -> usize {
    if d.get(0..4) == Some(b"ttcf") {
        u32_at(d, 8).unwrap_or(0) as usize
    } else {
        1
    }
}

fn face_offset(d: &[u8], index: u32) -> Option<usize> {
    if d.get(0..4) == Some(b"ttcf") {
        let n = u32_at(d, 8)?;
        if index >= n {
            return None;
        }
        Some(u32_at(d, 12 + 4 * index as usize)? as usize)
    } else if index == 0 {
        Some(0)
    } else {
        None
    }
}

impl Font {
    pub fn open(path: &FsPath, index: u32) -> Result<Font, String> {
        Font::open_weight(path, index, None)
    }

    /// Like `open`; a variable font is set to `weight` (100..900) if it has a
    /// weight axis, `None` leaves it at its default instance.
    pub fn open_weight(path: &FsPath, index: u32, weight: Option<u16>) -> Result<Font, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let map = Map::open_ro(&f).map_err(|e| format!("{}: {e}", path.display()))?;
        Font::from_data_weight(Arc::new(FontData::Mapped(map)), index, weight)
    }

    pub fn from_bytes(bytes: Vec<u8>, index: u32) -> Result<Font, String> {
        Font::from_data(Arc::new(FontData::Owned(bytes)), index)
    }

    pub fn from_bytes_weight(bytes: Vec<u8>, index: u32, weight: Option<u16>) -> Result<Font, String> {
        Font::from_data_weight(Arc::new(FontData::Owned(bytes)), index, weight)
    }

    pub fn from_data(data: Arc<FontData>, index: u32) -> Result<Font, String> {
        Font::from_data_weight(data, index, None)
    }

    pub fn from_data_weight(data: Arc<FontData>, index: u32, want_weight: Option<u16>) -> Result<Font, String> {
        let d = data.bytes();
        let off = face_offset(d, index).ok_or("no such face")?;
        let magic = d.get(off..off + 4).ok_or("truncated font")?;
        if !(magic == [0, 1, 0, 0] || magic == *b"OTTO" || magic == *b"true" || magic == *b"typ1") {
            return Err("not a TrueType/OpenType font".into());
        }
        let tables = read_tables(d, off).ok_or("bad table directory")?;
        let t = |tag: &[u8; 4]| tables.iter().find(|(k, _)| k == tag).map(|(_, r)| *r);

        let head = t(b"head").ok_or("no head table")?;
        let upem = u16_at(d, head.0 + 18).ok_or("bad head")?;
        if upem == 0 {
            return Err("units per em is zero".into());
        }
        let long_loca = i16_at(d, head.0 + 50).unwrap_or(0) != 0;
        let maxp = t(b"maxp").ok_or("no maxp table")?;
        let num_glyphs = u16_at(d, maxp.0 + 4).ok_or("bad maxp")? as u32;

        let hhea = t(b"hhea").ok_or("no hhea table")?;
        let mut ascent = i16_at(d, hhea.0 + 4).unwrap_or(0) as f32;
        let mut descent = i16_at(d, hhea.0 + 6).unwrap_or(0) as f32;
        let mut line_gap = i16_at(d, hhea.0 + 8).unwrap_or(0) as f32;
        let num_hmetrics = u16_at(d, hhea.0 + 34).unwrap_or(0) as usize;
        let hmtx = t(b"hmtx").ok_or("no hmtx table")?;

        let (mut weight, mut italic) = (400u16, false);
        if let Some(os2) = t(b"OS/2") {
            weight = u16_at(d, os2.0 + 4).unwrap_or(400);
            let sel = u16_at(d, os2.0 + 62).unwrap_or(0);
            italic = sel & 1 != 0;
            // USE_TYPO_METRICS: the designer's preferred line metrics
            if sel & (1 << 7) != 0 && os2.1 >= 74 {
                let (a, dsc, g) = (
                    i16_at(d, os2.0 + 68).unwrap_or(0) as f32,
                    i16_at(d, os2.0 + 70).unwrap_or(0) as f32,
                    i16_at(d, os2.0 + 72).unwrap_or(0) as f32,
                );
                if a - dsc > 0.0 {
                    ascent = a;
                    descent = dsc;
                    line_gap = g;
                }
            }
        }
        if ascent - descent <= 0.0 {
            ascent = upem as f32 * 0.8;
            descent = -(upem as f32) * 0.2;
        }

        // variable fonts: where in the weight axis to sit
        let axes = t(b"fvar").and_then(|r| d.get(r.0..r.0 + r.1)).map(var::read_axes).unwrap_or_default();
        let wght = var::weight_range(&axes);
        let coords = match (want_weight, wght) {
            (Some(w), Some(_)) => {
                let avar = t(b"avar").and_then(|r| d.get(r.0..r.0 + r.1));
                var::normalize_weight(&axes, avar, w as f32)
            }
            _ => vec![0.0; axes.len()],
        };
        let mut variable = false;
        if let (Some(w), Some((lo, hi))) = (want_weight, wght) {
            weight = (w as f32).max(lo).min(hi).round() as u16;
        }

        let outlines = if let (Some(glyf), Some(loca)) = (t(b"glyf"), t(b"loca")) {
            Outlines::Glyf { loca, glyf, long_loca }
        } else if let Some(c) = t(b"CFF ") {
            let cff = Cff::parse(d.get(c.0..c.0 + c.1).ok_or("bad CFF range")?).ok_or("unsupported CFF table")?;
            Outlines::Cff { table: c, cff }
        } else if let Some(c) = t(b"CFF2") {
            let mut cff = Cff::parse(d.get(c.0..c.0 + c.1).ok_or("bad CFF2 range")?).ok_or("unsupported CFF2 table")?;
            cff.set_coords(&coords);
            variable = cff.is_variable();
            Outlines::Cff { table: c, cff }
        } else {
            return Err("no outlines (bitmap-only fonts are not supported)".into());
        };
        let hvar = if variable { t(b"HVAR").and_then(|r| Hvar::parse(d, r.0)) } else { None };
        let hvar_scalars = hvar.as_ref().map(|h| h.region_scalars(&coords)).unwrap_or_default();

        let cmap = t(b"cmap").and_then(|r| pick_cmap(d, r.0));
        let (family, style) = t(b"name").map(|r| read_names(d, r)).unwrap_or_default();

        Ok(Font {
            data,
            units_per_em: upem as f32,
            num_glyphs,
            ascent,
            descent,
            line_gap,
            hmtx,
            num_hmetrics,
            cmap,
            outlines,
            family,
            style,
            weight,
            italic,
            variable,
            hvar,
            hvar_scalars,
        })
    }

    fn d(&self) -> &[u8] {
        self.data.bytes()
    }

    /// Glyph id for a character, 0 if the font has none.
    pub fn glyph_index(&self, ch: char) -> u32 {
        let Some(cm) = self.cmap else { return 0 };
        let d = self.d();
        let c = ch as u32;
        let g = match cm.format {
            4 => lookup_format4(d, cm.offset, c),
            12 => lookup_format12(d, cm.offset, c),
            0 => {
                if c < 256 {
                    d.get(cm.offset + 6 + c as usize).copied().unwrap_or(0) as u32
                } else {
                    0
                }
            }
            6 => {
                let first = u16_at(d, cm.offset + 6).unwrap_or(0) as u32;
                let count = u16_at(d, cm.offset + 8).unwrap_or(0) as u32;
                if c >= first && c < first + count {
                    u16_at(d, cm.offset + 10 + 2 * (c - first) as usize).unwrap_or(0) as u32
                } else {
                    0
                }
            }
            _ => 0,
        };
        if g < self.num_glyphs {
            g
        } else {
            0
        }
    }

    /// Horizontal advance in font units.
    pub fn advance(&self, gid: u32) -> f32 {
        if self.num_hmetrics == 0 {
            return self.units_per_em * 0.5;
        }
        let i = (gid as usize).min(self.num_hmetrics - 1);
        let base = u16_at(self.d(), self.hmtx.0 + 4 * i).unwrap_or(0) as f32;
        match &self.hvar {
            Some(h) => (base + h.advance_delta(self.d(), &self.hvar_scalars, gid as usize)).round().max(0.0),
            None => base,
        }
    }

    /// Outline in font units, y up. Empty glyphs (space) give an empty path.
    pub fn outline(&self, gid: u32) -> Option<Path> {
        if gid >= self.num_glyphs {
            return None;
        }
        match &self.outlines {
            Outlines::Glyf { .. } => self.glyf_outline(gid, 0),
            Outlines::Cff { table, cff } => {
                let d = self.d().get(table.0..table.0 + table.1)?;
                cff.glyph(d, gid as usize)
            }
        }
    }

    fn glyf_outline(&self, gid: u32, depth: usize) -> Option<Path> {
        let Outlines::Glyf { loca, glyf, long_loca } = &self.outlines else { return None };
        let d = self.d();
        let (a, b) = if *long_loca {
            (u32_at(d, loca.0 + 4 * gid as usize)? as usize, u32_at(d, loca.0 + 4 * gid as usize + 4)? as usize)
        } else {
            (
                u16_at(d, loca.0 + 2 * gid as usize)? as usize * 2,
                u16_at(d, loca.0 + 2 * gid as usize + 2)? as usize * 2,
            )
        };
        if b <= a {
            return Some(Path::new()); // no outline
        }
        if b > glyf.1 {
            return None;
        }
        let g = d.get(glyf.0 + a..glyf.0 + b)?;
        let ncont = i16_at(g, 0)?;
        if ncont >= 0 {
            simple_glyph(g, ncont as usize)
        } else {
            if depth >= 6 {
                return None;
            }
            self.composite_glyph(g, depth)
        }
    }

    fn composite_glyph(&self, g: &[u8], depth: usize) -> Option<Path> {
        let mut out = Path::new();
        let mut p = 10;
        for _ in 0..64 {
            let flags = u16_at(g, p)?;
            let child = u16_at(g, p + 2)? as u32;
            p += 4;
            let (dx, dy);
            if flags & 1 != 0 {
                dx = i16_at(g, p)? as f32;
                dy = i16_at(g, p + 2)? as f32;
                p += 4;
            } else {
                dx = *g.get(p)? as i8 as f32;
                dy = *g.get(p + 1)? as i8 as f32;
                p += 2;
            }
            let f2 = |o: usize| i16_at(g, o).map(|v| v as f32 / 16384.0);
            let (mut a, mut b, mut c, mut dd) = (1.0, 0.0, 0.0, 1.0);
            if flags & 0x8 != 0 {
                a = f2(p)?;
                dd = a;
                p += 2;
            } else if flags & 0x40 != 0 {
                a = f2(p)?;
                dd = f2(p + 2)?;
                p += 4;
            } else if flags & 0x80 != 0 {
                a = f2(p)?;
                b = f2(p + 2)?;
                c = f2(p + 4)?;
                dd = f2(p + 6)?;
                p += 8;
            }
            // flag 0x2: the arguments are x/y offsets (point matching is not supported)
            let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
            if let Some(cp) = self.glyf_outline(child, depth + 1) {
                out.append_transformed(&cp, [a, b, c, dd, dx, dy]);
            }
            if flags & 0x20 == 0 {
                break;
            }
        }
        Some(out)
    }
}

fn simple_glyph(g: &[u8], ncont: usize) -> Option<Path> {
    if ncont == 0 {
        return Some(Path::new());
    }
    let mut ends = Vec::with_capacity(ncont);
    for i in 0..ncont {
        ends.push(u16_at(g, 10 + 2 * i)? as usize);
    }
    let npts = ends.last()? + 1;
    if npts > 20000 || ends.windows(2).any(|w| w[1] < w[0]) {
        return None;
    }
    let ilen = u16_at(g, 10 + 2 * ncont)? as usize;
    let mut p = 10 + 2 * ncont + 2 + ilen;

    let mut flags = Vec::with_capacity(npts);
    while flags.len() < npts {
        let f = *g.get(p)?;
        p += 1;
        flags.push(f);
        if f & 8 != 0 {
            let rep = *g.get(p)? as usize;
            p += 1;
            for _ in 0..rep {
                if flags.len() < npts {
                    flags.push(f);
                }
            }
        }
    }
    let mut xs = Vec::with_capacity(npts);
    let mut v = 0i32;
    for &f in &flags {
        if f & 2 != 0 {
            let dx = *g.get(p)? as i32;
            p += 1;
            v += if f & 16 != 0 { dx } else { -dx };
        } else if f & 16 == 0 {
            v += i16_at(g, p)? as i32;
            p += 2;
        }
        xs.push(v);
    }
    let mut ys = Vec::with_capacity(npts);
    v = 0;
    for &f in &flags {
        if f & 4 != 0 {
            let dy = *g.get(p)? as i32;
            p += 1;
            v += if f & 32 != 0 { dy } else { -dy };
        } else if f & 32 == 0 {
            v += i16_at(g, p)? as i32;
            p += 2;
        }
        ys.push(v);
    }

    let mut path = Path::new();
    let mut start = 0;
    for &end in &ends {
        let pts: Vec<(f32, f32, bool)> =
            (start..=end).map(|i| (xs[i] as f32, ys[i] as f32, flags[i] & 1 != 0)).collect();
        start = end + 1;
        contour(&mut path, &pts);
    }
    Some(path)
}

/// One TrueType contour (quadratic, implied on-curve points) into the path.
fn contour(path: &mut Path, pts: &[(f32, f32, bool)]) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    let mid = |a: (f32, f32, bool), b: (f32, f32, bool)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let (start, first, count): ((f32, f32), usize, usize) = if pts[0].2 {
        ((pts[0].0, pts[0].1), 1, n - 1)
    } else if pts[n - 1].2 {
        ((pts[n - 1].0, pts[n - 1].1), 0, n - 1)
    } else {
        (mid(pts[n - 1], pts[0]), 0, n)
    };
    path.move_to(start.0, start.1);
    let mut ctrl: Option<(f32, f32)> = None;
    for k in 0..count {
        let p = pts[first + k];
        if p.2 {
            match ctrl.take() {
                Some(c) => path.quad_to(c.0, c.1, p.0, p.1),
                None => path.line_to(p.0, p.1),
            };
        } else {
            if let Some(c) = ctrl {
                let m = ((c.0 + p.0) / 2.0, (c.1 + p.1) / 2.0);
                path.quad_to(c.0, c.1, m.0, m.1);
            }
            ctrl = Some((p.0, p.1));
        }
    }
    if let Some(c) = ctrl {
        path.quad_to(c.0, c.1, start.0, start.1);
    }
    path.close();
}

// ---------------------------------------------------------------- cmap

fn pick_cmap(d: &[u8], table: usize) -> Option<Cmap> {
    let n = u16_at(d, table + 2)? as usize;
    // (priority, offset): lower is better
    let mut best: Option<(u8, usize)> = None;
    for i in 0..n {
        let r = table + 4 + i * 8;
        let (plat, enc) = (u16_at(d, r)?, u16_at(d, r + 2)?);
        let off = table + u32_at(d, r + 4)? as usize;
        let fmt = u16_at(d, off)?;
        let prio = match (plat, enc, fmt) {
            (3, 10, 12) => 0,
            (0, 4 | 6, 12) => 1,
            (3, 1, 4) => 2,
            (0, _, 12) => 3,
            (0, _, 4) => 4,
            (_, _, 12) => 5,
            (1, 0, 0) => 8,
            _ => continue,
        };
        if best.map(|(p, _)| prio < p).unwrap_or(true) {
            best = Some((prio, off));
        }
    }
    let (_, offset) = best?;
    Some(Cmap { offset, format: u16_at(d, offset)? })
}

fn lookup_format4(d: &[u8], t: usize, c: u32) -> u32 {
    if c > 0xffff {
        return 0;
    }
    let c = c as u16;
    let Some(seg_x2) = u16_at(d, t + 6) else { return 0 };
    let segs = seg_x2 as usize / 2;
    let end_codes = t + 14;
    let start_codes = end_codes + seg_x2 as usize + 2;
    let deltas = start_codes + seg_x2 as usize;
    let range_offs = deltas + seg_x2 as usize;
    // first segment whose end code is >= c
    let (mut lo, mut hi) = (0usize, segs);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match u16_at(d, end_codes + 2 * mid) {
            Some(e) if e < c => lo = mid + 1,
            Some(_) => hi = mid,
            None => return 0,
        }
    }
    if lo >= segs {
        return 0;
    }
    let (Some(start), Some(delta), Some(ro)) =
        (u16_at(d, start_codes + 2 * lo), u16_at(d, deltas + 2 * lo), u16_at(d, range_offs + 2 * lo))
    else {
        return 0;
    };
    if c < start {
        return 0;
    }
    if ro == 0 {
        return c.wrapping_add(delta) as u32;
    }
    let addr = range_offs + 2 * lo + ro as usize + 2 * (c - start) as usize;
    match u16_at(d, addr) {
        Some(0) | None => 0,
        Some(g) => g.wrapping_add(delta) as u32,
    }
}

fn lookup_format12(d: &[u8], t: usize, c: u32) -> u32 {
    let Some(n) = u32_at(d, t + 12) else { return 0 };
    let groups = t + 16;
    let (mut lo, mut hi) = (0usize, n as usize);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (Some(s), Some(e)) = (u32_at(d, groups + 12 * mid), u32_at(d, groups + 12 * mid + 4)) else { return 0 };
        if c < s {
            hi = mid;
        } else if c > e {
            lo = mid + 1;
        } else {
            return u32_at(d, groups + 12 * mid + 8).map(|g| g + (c - s)).unwrap_or(0);
        }
    }
    0
}

// ---------------------------------------------------------------- names

/// (family, style) from the `name` table; prefers the typographic names.
fn read_names(d: &[u8], table: Range) -> (String, String) {
    let t = table.0;
    let Some(count) = u16_at(d, t + 2) else { return Default::default() };
    let Some(str_off) = u16_at(d, t + 4) else { return Default::default() };
    let mut found: [Option<(u8, String)>; 4] = [None, None, None, None]; // 1, 2, 16, 17
    for i in 0..count as usize {
        let r = t + 6 + i * 12;
        let (Some(plat), Some(enc), Some(lang), Some(id), Some(len), Some(off)) = (
            u16_at(d, r),
            u16_at(d, r + 2),
            u16_at(d, r + 4),
            u16_at(d, r + 6),
            u16_at(d, r + 8),
            u16_at(d, r + 10),
        ) else {
            break;
        };
        let slot = match id {
            1 => 0,
            2 => 1,
            16 => 2,
            17 => 3,
            _ => continue,
        };
        let Some(raw) = d.get(t + str_off as usize + off as usize..t + str_off as usize + off as usize + len as usize) else { continue };
        // Windows/Unicode names are UTF-16BE; Mac names are (mostly) ASCII
        let (prio, s) = match (plat, enc) {
            (3, _) | (0, _) => {
                let u: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                let en = if plat == 3 && lang == 0x409 { 0 } else { 1 };
                (en, String::from_utf16_lossy(&u))
            }
            (1, 0) => (2, raw.iter().map(|&b| b as char).collect()),
            _ => continue,
        };
        if found[slot].as_ref().map(|(p, _)| prio < *p).unwrap_or(true) {
            found[slot] = Some((prio, s));
        }
    }
    let get = |i: usize| found[i].as_ref().map(|(_, s)| s.clone());
    (get(2).or_else(|| get(0)).unwrap_or_default(), get(3).or_else(|| get(1)).unwrap_or_default())
}

/// Read just enough of a font file to describe each face in it, without
/// mapping or reading the whole file. Used to build the system font list.
pub fn probe(path: &FsPath) -> Vec<FaceInfo> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let mut head = [0u8; 16];
    if f.read_exact(&mut head).is_err() {
        return vec![];
    }
    let offsets: Vec<u64> = if &head[0..4] == b"ttcf" {
        let n = u32::from_be_bytes([head[8], head[9], head[10], head[11]]).min(64) as usize;
        let mut buf = vec![0u8; 4 * n];
        if f.seek(SeekFrom::Start(12)).is_err() || f.read_exact(&mut buf).is_err() {
            return vec![];
        }
        buf.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]) as u64).collect()
    } else if head[0..4] == [0, 1, 0, 0] || &head[0..4] == b"OTTO" || &head[0..4] == b"true" {
        vec![0]
    } else {
        return vec![];
    };

    let mut out = Vec::new();
    for (index, off) in offsets.into_iter().enumerate() {
        let mut dir = [0u8; 12];
        if f.seek(SeekFrom::Start(off)).is_err() || f.read_exact(&mut dir).is_err() {
            continue;
        }
        let n = u16::from_be_bytes([dir[4], dir[5]]) as usize;
        if n == 0 || n > 128 {
            continue;
        }
        let mut recs = vec![0u8; n * 16];
        if f.read_exact(&mut recs).is_err() {
            continue;
        }
        let find = |tag: &[u8; 4]| {
            recs.chunks_exact(16).find(|r| &r[0..4] == tag).map(|r| {
                (
                    u32::from_be_bytes([r[8], r[9], r[10], r[11]]) as u64,
                    u32::from_be_bytes([r[12], r[13], r[14], r[15]]) as usize,
                )
            })
        };
        let Some(name) = find(b"name") else { continue };
        if name.1 > 1 << 20 {
            continue;
        }
        let mut nbuf = vec![0u8; name.1];
        if f.seek(SeekFrom::Start(name.0)).is_err() || f.read_exact(&mut nbuf).is_err() {
            continue;
        }
        let (family, style) = read_names(&nbuf, (0, nbuf.len()));
        let (mut weight, mut italic) = (400, false);
        if let Some(os2) = find(b"OS/2") {
            let mut b = [0u8; 64];
            if os2.1 >= 64 && f.seek(SeekFrom::Start(os2.0)).is_ok() && f.read_exact(&mut b).is_ok() {
                weight = u16::from_be_bytes([b[4], b[5]]);
                italic = u16::from_be_bytes([b[62], b[63]]) & 1 != 0;
            }
        }
        if family.is_empty() {
            continue;
        }
        out.push(FaceInfo { path: path.to_path_buf(), index: index as u32, family, style, weight, italic, variable: find(b"fvar").is_some() });
    }
    out
}

/// Description of one face found on disk.
#[derive(Clone, Debug)]
pub struct FaceInfo {
    pub path: std::path::PathBuf,
    pub index: u32,
    pub family: String,
    pub style: String,
    pub weight: u16,
    pub italic: bool,
    /// has an `fvar` table: its weight can be chosen
    pub variable: bool,
}
