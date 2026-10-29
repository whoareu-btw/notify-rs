//! CFF outlines: the Type 2 charstring interpreter used by OpenType fonts with
//! `CFF ` tables (Inter, static Noto CJK, ...) and with `CFF2` tables (variable
//! fonts such as the Noto Sans CJK that NixOS ships). Name-keyed and CID-keyed
//! fonts are handled. CFF2 `blend` is evaluated at fixed coordinates chosen
//! with `set_coords`, so a variable font is drawn at one weight.

use super::raster::Path;
use super::var::VarStore;

const MAX_STACK: usize = 96;
/// CFF2 charstrings carry blend deltas on the stack
const MAX_STACK_V2: usize = 513;
const MAX_SUBR_DEPTH: usize = 10;
const OP_BUDGET: usize = 200_000;

fn be16(d: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]) as usize)
}

#[derive(Clone, Copy, Debug)]
struct Index {
    count: usize,
    off_size: usize,
    /// position of the offset array
    offs: usize,
    /// position just before the first data byte (offsets are 1-based)
    base: usize,
    /// first byte after the whole INDEX
    end: usize,
}

fn parse_index(d: &[u8], pos: usize) -> Option<Index> {
    parse_index_w(d, pos, false)
}

/// CFF2 INDEXes count their entries with 32 bits instead of 16.
fn parse_index_w(d: &[u8], pos: usize, wide: bool) -> Option<Index> {
    let (count, cw) = if wide {
        (u32::from_be_bytes([*d.get(pos)?, *d.get(pos + 1)?, *d.get(pos + 2)?, *d.get(pos + 3)?]) as usize, 4)
    } else {
        (be16(d, pos)?, 2)
    };
    if count == 0 {
        return Some(Index { count: 0, off_size: 1, offs: pos + cw, base: pos + cw, end: pos + cw });
    }
    if count > d.len() {
        return None;
    }
    let off_size = *d.get(pos + cw)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offs = pos + cw + 1;
    let base = offs + (count + 1) * off_size - 1;
    let last = read_off(d, offs, off_size, count)?;
    let end = base + last;
    if end > d.len() {
        return None;
    }
    Some(Index { count, off_size, offs, base, end })
}

fn read_off(d: &[u8], offs: usize, size: usize, i: usize) -> Option<usize> {
    let p = offs + i * size;
    let b = d.get(p..p + size)?;
    Some(b.iter().fold(0usize, |a, &x| (a << 8) | x as usize))
}

fn index_get<'a>(d: &'a [u8], ix: &Index, i: usize) -> Option<&'a [u8]> {
    if i >= ix.count {
        return None;
    }
    let a = read_off(d, ix.offs, ix.off_size, i)?;
    let b = read_off(d, ix.offs, ix.off_size, i + 1)?;
    if a == 0 || b < a {
        return None;
    }
    d.get(ix.base + a..ix.base + b)
}

/// DICT data: (operator, operands). Two-byte operators are 0x0c00 | second byte.
fn parse_dict(d: &[u8]) -> Vec<(u16, Vec<f32>)> {
    let mut out = Vec::new();
    let mut ops: Vec<f32> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let b0 = d[i];
        i += 1;
        match b0 {
            // 22..=24 are the CFF2 operators vsindex, blend and vstore
            0..=24 => {
                let op = if b0 == 12 {
                    let b1 = d.get(i).copied().unwrap_or(0);
                    i += 1;
                    0x0c00 | b1 as u16
                } else {
                    b0 as u16
                };
                out.push((op, std::mem::take(&mut ops)));
            }
            28 => {
                if i + 2 > d.len() {
                    break;
                }
                ops.push(i16::from_be_bytes([d[i], d[i + 1]]) as f32);
                i += 2;
            }
            29 => {
                if i + 4 > d.len() {
                    break;
                }
                ops.push(i32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as f32);
                i += 4;
            }
            30 => {
                let mut s = String::new();
                'nib: while i < d.len() {
                    let b = d[i];
                    i += 1;
                    for n in [b >> 4, b & 15] {
                        match n {
                            0..=9 => s.push((b'0' + n) as char),
                            10 => s.push('.'),
                            11 => s.push('E'),
                            12 => s.push_str("E-"),
                            14 => s.push('-'),
                            15 => break 'nib,
                            _ => {}
                        }
                    }
                }
                ops.push(s.parse().unwrap_or(0.0));
            }
            32..=246 => ops.push(b0 as f32 - 139.0),
            247..=250 => {
                let b1 = d.get(i).copied().unwrap_or(0) as f32;
                i += 1;
                ops.push((b0 as f32 - 247.0) * 256.0 + b1 + 108.0);
            }
            251..=254 => {
                let b1 = d.get(i).copied().unwrap_or(0) as f32;
                i += 1;
                ops.push(-(b0 as f32 - 251.0) * 256.0 - b1 - 108.0);
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
struct Private {
    subrs: Option<Index>,
    /// CFF2: which delta set `blend` uses until the charstring says otherwise
    vsindex: usize,
}

#[derive(Debug)]
enum FdSelect {
    None,
    Format0(usize),
    Format3 { start: usize, n: usize },
    /// CFF2: 32-bit ranges
    Format4 { start: usize, n: usize },
}

#[derive(Debug)]
pub struct Cff {
    char_strings: Index,
    gsubrs: Index,
    privates: Vec<Private>,
    fd_select: FdSelect,
    v2: bool,
    vstore: Option<VarStore>,
    /// per delta set, the scalar of each of its regions at the chosen coordinates
    scalars: Vec<Vec<f32>>,
}

fn be32(d: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_be_bytes([*d.get(o)?, *d.get(o + 1)?, *d.get(o + 2)?, *d.get(o + 3)?]) as usize)
}

fn read_private(d: &[u8], size_off: &[f32], wide: bool) -> Option<Private> {
    let (size, off) = (*size_off.first()? as usize, *size_off.get(1)? as usize);
    let pd = parse_dict(d.get(off..off.checked_add(size)?)?);
    let subrs = pd
        .iter()
        .find(|(o, _)| *o == 19)
        .and_then(|(_, v)| v.first().copied())
        .and_then(|rel| parse_index_w(d, off + rel as usize, wide));
    let vsindex = pd.iter().find(|(o, _)| *o == 22).and_then(|(_, v)| v.first().copied()).unwrap_or(0.0).max(0.0) as usize;
    Some(Private { subrs, vsindex })
}

impl Cff {
    /// `d` is the whole `CFF ` table.
    pub fn parse(d: &[u8]) -> Option<Cff> {
        match *d.first()? {
            1 => Cff::parse1(d),
            2 => Cff::parse2(d),
            _ => None,
        }
    }

    fn parse2(d: &[u8]) -> Option<Cff> {
        let hdr = *d.get(2)? as usize;
        let top_len = be16(d, 3)?;
        let top = parse_dict(d.get(hdr..hdr.checked_add(top_len)?)?);
        let get = |op: u16| top.iter().find(|(o, _)| *o == op).map(|(_, v)| v.as_slice());
        let gsubrs = parse_index_w(d, hdr + top_len, true)?;
        let char_strings = parse_index_w(d, *get(17)?.first()? as usize, true)?;
        let fd_ix = parse_index_w(d, *get(0x0c24)?.first()? as usize, true)?;
        let mut privates = Vec::with_capacity(fd_ix.count);
        for i in 0..fd_ix.count {
            let fd = parse_dict(index_get(d, &fd_ix, i)?);
            privates.push(fd.iter().find(|(o, _)| *o == 18).and_then(|(_, v)| read_private(d, v, true)).unwrap_or_default());
        }
        if privates.is_empty() {
            return None;
        }
        let fd_select = match get(0x0c25).and_then(|v| v.first()) {
            None => FdSelect::None,
            Some(fs) => {
                let fs = *fs as usize;
                match *d.get(fs)? {
                    0 => FdSelect::Format0(fs + 1),
                    3 => FdSelect::Format3 { start: fs + 3, n: be16(d, fs + 1)? },
                    4 => FdSelect::Format4 { start: fs + 5, n: be32(d, fs + 1)? },
                    _ => return None,
                }
            }
        };
        // the variation store is prefixed with its own 16-bit length
        let vstore = get(24).and_then(|v| v.first()).and_then(|&o| VarStore::parse(d, o as usize + 2));
        let mut cff = Cff { char_strings, gsubrs, privates, fd_select, v2: true, vstore, scalars: Vec::new() };
        cff.set_coords(&[]);
        Some(cff)
    }

    /// Choose where in the design space CFF2 outlines are evaluated
    /// (normalized coordinates, one per axis; missing ones count as 0).
    pub fn set_coords(&mut self, coords: &[f32]) {
        self.scalars.clear();
        if let Some(vs) = &self.vstore {
            let all = vs.region_scalars(coords);
            for i in 0..vs.data_sets() {
                self.scalars.push(vs.set_scalars(&all, i).unwrap_or_default());
            }
        }
    }

    pub fn is_variable(&self) -> bool {
        self.v2 && self.vstore.is_some()
    }

    fn parse1(d: &[u8]) -> Option<Cff> {
        let hdr_size = *d.get(2)? as usize;
        let name_ix = parse_index(d, hdr_size)?;
        let top_ix = parse_index(d, name_ix.end)?;
        let str_ix = parse_index(d, top_ix.end)?;
        let gsubrs = parse_index(d, str_ix.end)?;
        let top = parse_dict(index_get(d, &top_ix, 0)?);

        let get = |op: u16| top.iter().find(|(o, _)| *o == op).map(|(_, v)| v.as_slice());
        let cs_off = *get(17)?.first()? as usize;
        let char_strings = parse_index(d, cs_off)?;

        let read_private = |size_off: &[f32]| read_private(d, size_off, false);

        let mut privates = Vec::new();
        let mut fd_select = FdSelect::None;
        if let Some(fda) = get(0x0c24).and_then(|v| v.first()) {
            // CID-keyed: one Private per Font DICT
            let fd_ix = parse_index(d, *fda as usize)?;
            for i in 0..fd_ix.count {
                let fd = parse_dict(index_get(d, &fd_ix, i)?);
                let p = fd.iter().find(|(o, _)| *o == 18).and_then(|(_, v)| read_private(v)).unwrap_or_default();
                privates.push(p);
            }
            if let Some(fs) = get(0x0c25).and_then(|v| v.first()) {
                let fs = *fs as usize;
                fd_select = match *d.get(fs)? {
                    0 => FdSelect::Format0(fs + 1),
                    3 => FdSelect::Format3 { start: fs + 3, n: be16(d, fs + 1)? },
                    _ => return None,
                };
            }
        } else {
            privates.push(get(18).and_then(read_private).unwrap_or_default());
        }
        if privates.is_empty() {
            privates.push(Private::default());
        }
        Some(Cff { char_strings, gsubrs, privates, fd_select, v2: false, vstore: None, scalars: Vec::new() })
    }

    pub fn num_glyphs(&self) -> usize {
        self.char_strings.count
    }

    fn fd_for(&self, d: &[u8], gid: usize) -> usize {
        let fd = match self.fd_select {
            FdSelect::None => 0,
            FdSelect::Format0(p) => d.get(p + gid).copied().unwrap_or(0) as usize,
            FdSelect::Format3 { start, n } => {
                let mut fd = 0;
                for i in 0..n {
                    let first = be16(d, start + i * 3).unwrap_or(usize::MAX);
                    let next = be16(d, start + (i + 1) * 3).unwrap_or(usize::MAX);
                    if gid >= first && gid < next {
                        fd = d.get(start + i * 3 + 2).copied().unwrap_or(0) as usize;
                        break;
                    }
                }
                fd
            }
            FdSelect::Format4 { start, n } => {
                let mut fd = 0;
                for i in 0..n {
                    let first = be32(d, start + i * 6).unwrap_or(usize::MAX);
                    let next = be32(d, start + (i + 1) * 6).unwrap_or(usize::MAX);
                    if gid >= first && gid < next {
                        fd = be16(d, start + i * 6 + 4).unwrap_or(0);
                        break;
                    }
                }
                fd
            }
        };
        fd.min(self.privates.len() - 1)
    }

    /// Outline in font units, y up. `d` is the whole `CFF ` table.
    pub fn glyph(&self, d: &[u8], gid: usize) -> Option<Path> {
        let code = index_get(d, &self.char_strings, gid)?;
        let private = &self.privates[self.fd_for(d, gid)];
        let local = private.subrs;
        let mut it = Interp {
            d,
            gsubrs: self.gsubrs,
            lsubrs: local,
            v2: self.v2,
            scalars: &self.scalars,
            vsindex: private.vsindex,
            stack: Vec::with_capacity(if self.v2 { MAX_STACK_V2 } else { MAX_STACK }),
            x: 0.0,
            y: 0.0,
            path: Path::new(),
            open: false,
            nstems: 0,
            width_done: false,
            budget: OP_BUDGET,
            finished: false,
        };
        it.run(code, 0)?;
        if it.open {
            it.path.close();
        }
        Some(it.path)
    }
}

fn bias(n: usize) -> i32 {
    if n < 1240 {
        107
    } else if n < 33900 {
        1131
    } else {
        32768
    }
}

struct Interp<'a> {
    d: &'a [u8],
    gsubrs: Index,
    lsubrs: Option<Index>,
    v2: bool,
    scalars: &'a [Vec<f32>],
    vsindex: usize,
    stack: Vec<f32>,
    x: f32,
    y: f32,
    path: Path,
    open: bool,
    nstems: usize,
    width_done: bool,
    budget: usize,
    finished: bool,
}

impl Interp<'_> {
    fn push(&mut self, v: f32) -> Option<()> {
        if self.stack.len() >= if self.v2 { MAX_STACK_V2 } else { MAX_STACK } {
            return None;
        }
        self.stack.push(v);
        Some(())
    }

    /// The first stack-clearing operator may carry the glyph width as an extra
    /// leading argument. We take our widths from `hmtx`, so just drop it.
    fn take_width(&mut self, has_extra: bool) {
        if self.v2 {
            return; // CFF2 charstrings carry no width
        }
        if !self.width_done {
            if has_extra && !self.stack.is_empty() {
                self.stack.remove(0);
            }
            self.width_done = true;
        }
    }

    fn move_to(&mut self, dx: f32, dy: f32) {
        if self.open {
            self.path.close();
        }
        self.x += dx;
        self.y += dy;
        self.path.move_to(self.x, self.y);
        self.open = true;
    }

    fn line(&mut self, dx: f32, dy: f32) {
        self.x += dx;
        self.y += dy;
        self.path.line_to(self.x, self.y);
    }

    fn curve(&mut self, d1: (f32, f32), d2: (f32, f32), d3: (f32, f32)) {
        let (c1x, c1y) = (self.x + d1.0, self.y + d1.1);
        let (c2x, c2y) = (c1x + d2.0, c1y + d2.1);
        let (x, y) = (c2x + d3.0, c2y + d3.1);
        self.path.cubic_to(c1x, c1y, c2x, c2y, x, y);
        self.x = x;
        self.y = y;
    }

    fn run(&mut self, code: &[u8], depth: usize) -> Option<()> {
        let mut i = 0;
        while i < code.len() && !self.finished {
            if self.budget == 0 {
                return None;
            }
            self.budget -= 1;
            let b0 = code[i];
            i += 1;
            match b0 {
                28 => {
                    let v = i16::from_be_bytes([*code.get(i)?, *code.get(i + 1)?]);
                    i += 2;
                    self.push(v as f32)?;
                }
                32..=246 => self.push(b0 as f32 - 139.0)?,
                247..=250 => {
                    let b1 = *code.get(i)? as f32;
                    i += 1;
                    self.push((b0 as f32 - 247.0) * 256.0 + b1 + 108.0)?;
                }
                251..=254 => {
                    let b1 = *code.get(i)? as f32;
                    i += 1;
                    self.push(-(b0 as f32 - 251.0) * 256.0 - b1 - 108.0)?;
                }
                255 => {
                    let v = i32::from_be_bytes([*code.get(i)?, *code.get(i + 1)?, *code.get(i + 2)?, *code.get(i + 3)?]);
                    i += 4;
                    self.push(v as f32 / 65536.0)?;
                }
                1 | 3 | 18 | 23 => {
                    let n = self.stack.len();
                    self.take_width(n % 2 == 1);
                    self.nstems += self.stack.len() / 2;
                    self.stack.clear();
                }
                19 | 20 => {
                    let n = self.stack.len();
                    self.take_width(n % 2 == 1);
                    self.nstems += self.stack.len() / 2;
                    self.stack.clear();
                    i += (self.nstems + 7) / 8;
                }
                21 => {
                    let n = self.stack.len();
                    self.take_width(n > 2);
                    let (dx, dy) = (*self.stack.first()?, *self.stack.get(1)?);
                    self.move_to(dx, dy);
                    self.stack.clear();
                }
                22 => {
                    let n = self.stack.len();
                    self.take_width(n > 1);
                    let dx = *self.stack.first()?;
                    self.move_to(dx, 0.0);
                    self.stack.clear();
                }
                4 => {
                    let n = self.stack.len();
                    self.take_width(n > 1);
                    let dy = *self.stack.first()?;
                    self.move_to(0.0, dy);
                    self.stack.clear();
                }
                5 => {
                    let a = std::mem::take(&mut self.stack);
                    for c in a.chunks_exact(2) {
                        self.line(c[0], c[1]);
                    }
                }
                6 | 7 => {
                    let a = std::mem::take(&mut self.stack);
                    let mut horiz = b0 == 6;
                    for &v in &a {
                        if horiz {
                            self.line(v, 0.0);
                        } else {
                            self.line(0.0, v);
                        }
                        horiz = !horiz;
                    }
                }
                8 => {
                    let a = std::mem::take(&mut self.stack);
                    for c in a.chunks_exact(6) {
                        self.curve((c[0], c[1]), (c[2], c[3]), (c[4], c[5]));
                    }
                }
                24 => {
                    // rcurveline: curves, then one line
                    let a = std::mem::take(&mut self.stack);
                    if a.len() < 2 {
                        return None;
                    }
                    let nc = (a.len() - 2) / 6;
                    for c in a[..nc * 6].chunks_exact(6) {
                        self.curve((c[0], c[1]), (c[2], c[3]), (c[4], c[5]));
                    }
                    let l = &a[nc * 6..];
                    if l.len() >= 2 {
                        self.line(l[0], l[1]);
                    }
                }
                25 => {
                    // rlinecurve: lines, then one curve
                    let a = std::mem::take(&mut self.stack);
                    if a.len() < 6 {
                        return None;
                    }
                    let nl = (a.len() - 6) / 2;
                    for c in a[..nl * 2].chunks_exact(2) {
                        self.line(c[0], c[1]);
                    }
                    let c = &a[nl * 2..];
                    if c.len() >= 6 {
                        self.curve((c[0], c[1]), (c[2], c[3]), (c[4], c[5]));
                    }
                }
                26 => {
                    // vvcurveto
                    let a = std::mem::take(&mut self.stack);
                    let (mut dx1, rest) = if a.len() % 2 == 1 { (a[0], &a[1..]) } else { (0.0, &a[..]) };
                    for c in rest.chunks_exact(4) {
                        self.curve((dx1, c[0]), (c[1], c[2]), (0.0, c[3]));
                        dx1 = 0.0;
                    }
                }
                27 => {
                    // hhcurveto
                    let a = std::mem::take(&mut self.stack);
                    let (mut dy1, rest) = if a.len() % 2 == 1 { (a[0], &a[1..]) } else { (0.0, &a[..]) };
                    for c in rest.chunks_exact(4) {
                        self.curve((c[0], dy1), (c[1], c[2]), (c[3], 0.0));
                        dy1 = 0.0;
                    }
                }
                30 | 31 => {
                    let a = std::mem::take(&mut self.stack);
                    let mut horiz = b0 == 31;
                    let mut k = 0;
                    while k + 4 <= a.len() {
                        let last = a.len() - k == 5;
                        let extra = if last { a[k + 4] } else { 0.0 };
                        if horiz {
                            self.curve((a[k], 0.0), (a[k + 1], a[k + 2]), (extra, a[k + 3]));
                        } else {
                            self.curve((0.0, a[k]), (a[k + 1], a[k + 2]), (a[k + 3], extra));
                        }
                        horiz = !horiz;
                        k += 4;
                    }
                }
                10 | 29 => {
                    let n = self.stack.pop()? as i32;
                    let (ix, bs) = if b0 == 10 {
                        let ix = self.lsubrs?;
                        (ix, bias(ix.count))
                    } else {
                        (self.gsubrs, bias(self.gsubrs.count))
                    };
                    let idx = n + bs;
                    if idx < 0 || depth >= MAX_SUBR_DEPTH {
                        return None;
                    }
                    let sub = index_get(self.d, &ix, idx as usize)?;
                    self.run(sub, depth + 1)?;
                }
                11 => return Some(()),
                15 if self.v2 => {
                    // vsindex: which delta set the following blends use
                    self.vsindex = self.stack.pop()?.max(0.0) as usize;
                }
                16 if self.v2 => self.blend()?,
                14 => {
                    let n = self.stack.len();
                    self.take_width(n == 1 || n == 5);
                    self.stack.clear();
                    self.finished = true;
                }
                12 => {
                    let b1 = *code.get(i)?;
                    i += 1;
                    self.escape(b1)?;
                }
                _ => {
                    // reserved operator: ignore but clear, like most renderers
                    self.stack.clear();
                }
            }
        }
        Some(())
    }

    /// `blend`: `n` default values, then `n * k` deltas (k deltas per value, one
    /// per region of the active delta set), then `n`. Leaves the `n` blended values.
    fn blend(&mut self) -> Option<()> {
        let n = self.stack.pop()?;
        if n < 0.0 || n > MAX_STACK_V2 as f32 {
            return None;
        }
        let n = n as usize;
        let sc = self.scalars.get(self.vsindex)?;
        let k = sc.len();
        let total = n.checked_mul(k + 1)?;
        if self.stack.len() < total {
            return None;
        }
        let base = self.stack.len() - total;
        for i in 0..n {
            let mut v = self.stack[base + i];
            for (j, s) in sc.iter().enumerate() {
                v += s * self.stack[base + n + i * k + j];
            }
            self.stack[base + i] = v;
        }
        self.stack.truncate(base + n);
        Some(())
    }

    fn escape(&mut self, b1: u8) -> Option<()> {
        match b1 {
            34 => {
                // hflex
                let a = std::mem::take(&mut self.stack);
                if a.len() < 7 {
                    return None;
                }
                self.curve((a[0], 0.0), (a[1], a[2]), (a[3], 0.0));
                self.curve((a[4], 0.0), (a[5], -a[2]), (a[6], 0.0));
            }
            35 => {
                // flex
                let a = std::mem::take(&mut self.stack);
                if a.len() < 12 {
                    return None;
                }
                self.curve((a[0], a[1]), (a[2], a[3]), (a[4], a[5]));
                self.curve((a[6], a[7]), (a[8], a[9]), (a[10], a[11]));
            }
            36 => {
                // hflex1
                let a = std::mem::take(&mut self.stack);
                if a.len() < 9 {
                    return None;
                }
                self.curve((a[0], a[1]), (a[2], a[3]), (a[4], 0.0));
                self.curve((a[5], 0.0), (a[6], a[7]), (a[8], -(a[1] + a[3] + a[7])));
            }
            37 => {
                // flex1
                let a = std::mem::take(&mut self.stack);
                if a.len() < 11 {
                    return None;
                }
                let dx = a[0] + a[2] + a[4] + a[6] + a[8];
                let dy = a[1] + a[3] + a[5] + a[7] + a[9];
                self.curve((a[0], a[1]), (a[2], a[3]), (a[4], a[5]));
                let (dx6, dy6) = if dx.abs() > dy.abs() { (a[10], -dy) } else { (-dx, a[10]) };
                self.curve((a[6], a[7]), (a[8], a[9]), (dx6, dy6));
            }
            9 => {
                let v = self.stack.pop()?;
                self.push(v.abs())?;
            }
            10 => {
                let (b, a) = (self.stack.pop()?, self.stack.pop()?);
                self.push(a + b)?;
            }
            11 => {
                let (b, a) = (self.stack.pop()?, self.stack.pop()?);
                self.push(a - b)?;
            }
            12 => {
                let (b, a) = (self.stack.pop()?, self.stack.pop()?);
                self.push(if b != 0.0 { a / b } else { 0.0 })?;
            }
            14 => {
                let v = self.stack.pop()?;
                self.push(-v)?;
            }
            24 => {
                let (b, a) = (self.stack.pop()?, self.stack.pop()?);
                self.push(a * b)?;
            }
            26 => {
                let v = self.stack.pop()?;
                self.push(v.max(0.0).sqrt())?;
            }
            18 => {
                self.stack.pop()?;
            }
            27 => {
                let v = *self.stack.last()?;
                self.push(v)?;
            }
            28 => {
                let n = self.stack.len();
                if n < 2 {
                    return None;
                }
                self.stack.swap(n - 1, n - 2);
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::raster::Cmd;

    /// Build a tiny CFF with one glyph whose charstring is `cs`.
    fn tiny_cff(cs: &[u8]) -> Vec<u8> {
        let mut d = vec![1, 0, 4, 1]; // header: major, minor, hdrSize, offSize
        // Name INDEX: one name "T"
        d.extend_from_slice(&[0, 1, 1, 1, 2, b'T']);
        // Top DICT INDEX: CharStrings offset (operand written as 29 + i32) then op 17
        let top_ix_pos = d.len();
        let mut top = vec![29, 0, 0, 0, 0, 17];
        let top_len = top.len();
        d.extend_from_slice(&[0, 1, 1, 1, (top_len + 1) as u8]);
        let top_at = d.len();
        d.extend_from_slice(&top);
        // String INDEX: empty
        d.extend_from_slice(&[0, 0]);
        // Global Subr INDEX: empty
        d.extend_from_slice(&[0, 0]);
        // CharStrings INDEX: .notdef (endchar) + our glyph
        let cs_pos = d.len();
        d.extend_from_slice(&[0, 2, 1, 1, 2, (2 + cs.len()) as u8, 14]);
        d.extend_from_slice(cs);
        let off = (cs_pos as i32).to_be_bytes();
        top[1..5].copy_from_slice(&off);
        d[top_at..top_at + top.len()].copy_from_slice(&top);
        let _ = top_ix_pos;
        d
    }

    fn glyph(cs: &[u8]) -> Option<Path> {
        let d = tiny_cff(cs);
        let cff = Cff::parse(&d)?;
        cff.glyph(&d, 1)
    }

    // charstring number helpers: -107..=107 are one byte (v + 139)
    fn n(v: i32) -> u8 {
        (v + 139) as u8
    }

    #[test]
    fn square_from_lines() {
        // 100 0 rmoveto, 50 hlineto, 50 vlineto, -50 hlineto, endchar
        let p = glyph(&[n(100), n(0), 21, n(50), 6, n(50), 7, n(-50), 6, 14]).unwrap();
        assert_eq!(
            p.cmds,
            vec![Cmd::Move(100.0, 0.0), Cmd::Line(150.0, 0.0), Cmd::Line(150.0, 50.0), Cmd::Line(100.0, 50.0), Cmd::Close]
        );
    }

    #[test]
    fn width_argument_is_skipped() {
        // width 500, then 10 20 rmoveto
        let p = glyph(&[28, 0x01, 0xf4, n(10), n(20), 21, n(5), 6, 14]).unwrap();
        assert_eq!(p.cmds[0], Cmd::Move(10.0, 20.0));
        assert_eq!(p.cmds[1], Cmd::Line(15.0, 20.0));
    }

    #[test]
    fn curves_and_hints() {
        // 10 20 hstem (2 stems args => one stem), hintmask (1 byte), moveto, rrcurveto
        let cs = [n(10), n(20), 1, 19, 0x80, n(0), n(0), 21, n(10), n(0), n(10), n(10), n(0), n(10), 8, 14];
        let p = glyph(&cs).unwrap();
        assert_eq!(p.cmds[1], Cmd::Cubic(10.0, 0.0, 20.0, 10.0, 20.0, 20.0));
    }

    #[test]
    fn hvcurveto_alternates() {
        // 0 0 rmoveto then two curves: dx1 dx2 dy2 dy3 | dya dxb dyb dxc
        let cs = [n(0), n(0), 21, n(10), n(10), n(10), n(10), n(10), n(10), n(10), n(10), 31, 14];
        let p = glyph(&cs).unwrap();
        // first curve starts horizontal: c1=(10,0) c2=(20,10) p=(20,20)
        assert_eq!(p.cmds[1], Cmd::Cubic(10.0, 0.0, 20.0, 10.0, 20.0, 20.0));
        // second starts vertical: c1=(20,30) c2=(30,40) p=(40,40)
        assert_eq!(p.cmds[2], Cmd::Cubic(20.0, 30.0, 30.0, 40.0, 40.0, 40.0));
    }

    #[test]
    fn local_subr_call() {
        // no local subrs in the tiny font: callsubr must fail cleanly, not panic
        assert!(glyph(&[n(0), 10, 14]).is_none());
    }

    #[test]
    fn garbage_charstrings_do_not_panic_or_hang() {
        // deterministic pseudo-random byte soup
        let mut s: u32 = 12345;
        for _ in 0..300 {
            let len = 1 + (s % 60) as usize;
            let mut cs = Vec::new();
            for _ in 0..len {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                cs.push((s >> 24) as u8);
            }
            let _ = glyph(&cs);
        }
        // endless recursion is cut off
        let d = tiny_cff(&[n(0), 29, 14]);
        let cff = Cff::parse(&d).unwrap();
        let _ = cff.glyph(&d, 1);
    }

    #[test]
    fn rejects_broken_tables() {
        assert!(Cff::parse(&[]).is_none());
        assert!(Cff::parse(&[2, 0, 5, 1]).is_none());
        assert!(Cff::parse(&[1, 0, 4, 1, 0xff, 0xff]).is_none());
    }

    // ---- CFF2, hand assembled so that every structure is exercised: two Font
    // DICTs with their own local subrs and default vsindex, global subrs,
    // FDSelect formats 0, 3 and 4, two delta sets with different region counts.

    fn i16b(v: i32) -> Vec<u8> {
        vec![28, (v >> 8) as u8, v as u8]
    }

    fn index32(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = (items.len() as u32).to_be_bytes().to_vec();
        if items.is_empty() {
            return out;
        }
        out.push(2); // offSize
        let mut off = 1u16;
        out.extend(off.to_be_bytes());
        for it in items {
            off += it.len() as u16;
            out.extend(off.to_be_bytes());
        }
        for it in items {
            out.extend(it);
        }
        out
    }

    /// One axis; regions [0,1,1] and [0,.5,1]; set 0 uses region 0, set 1 uses both.
    fn var_store() -> Vec<u8> {
        let f = |v: f32| ((v * 16384.0) as i16).to_be_bytes();
        let mut t = vec![0, 1];
        t.extend(16u32.to_be_bytes());
        t.extend(2u16.to_be_bytes());
        t.extend(32u32.to_be_bytes());
        t.extend(40u32.to_be_bytes());
        t.extend([0, 1, 0, 2]); // axisCount 1, regionCount 2
        for (a, b, c) in [(0.0f32, 1.0f32, 1.0f32), (0.0, 0.5, 1.0)] {
            t.extend(f(a));
            t.extend(f(b));
            t.extend(f(c));
        }
        t.extend([0, 0, 0, 0, 0, 1, 0, 0]); // set 0: no items, 1 region: 0
        t.extend([0, 0, 0, 0, 0, 2, 0, 0, 0, 1]); // set 1: no items, 2 regions: 0, 1
        t
    }

    const BLEND: u8 = 16;
    const VSINDEX: u8 = 15;
    const RMOVETO: u8 = 21;
    const HLINETO: u8 = 6;
    const VLINETO: u8 = 7;
    const CALLSUBR: u8 = 10;
    const CALLGSUBR: u8 = 29;
    const RETURN: u8 = 11;
    const HSTEMHM: u8 = 18;
    const HINTMASK: u8 = 19;

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// glyph -> font dict, for the FDSelect formats
    fn fd_select(format: u8, fds: &[u8]) -> Vec<u8> {
        match format {
            0 => [vec![0], fds.to_vec()].concat(),
            3 => {
                let mut ranges: Vec<(usize, u8)> = Vec::new();
                for (g, &fd) in fds.iter().enumerate() {
                    if ranges.last().map(|r| r.1) != Some(fd) {
                        ranges.push((g, fd));
                    }
                }
                let mut t = vec![3];
                t.extend((ranges.len() as u16).to_be_bytes());
                for (g, fd) in &ranges {
                    t.extend((*g as u16).to_be_bytes());
                    t.push(*fd);
                }
                t.extend((fds.len() as u16).to_be_bytes());
                t
            }
            _ => {
                let mut ranges: Vec<(usize, u8)> = Vec::new();
                for (g, &fd) in fds.iter().enumerate() {
                    if ranges.last().map(|r| r.1) != Some(fd) {
                        ranges.push((g, fd));
                    }
                }
                let mut t = vec![4];
                t.extend((ranges.len() as u32).to_be_bytes());
                for (g, fd) in &ranges {
                    t.extend((*g as u32).to_be_bytes());
                    t.extend((*fd as u16).to_be_bytes());
                }
                t.extend((fds.len() as u32).to_be_bytes());
                t
            }
        }
    }

    fn glyphs_v2() -> (Vec<Vec<u8>>, Vec<u8>) {
        let blend1 = |d: i32, delta: i32| cat(&[i16b(d), i16b(delta), i16b(1), vec![BLEND]]);
        let g: Vec<Vec<u8>> = vec![
            // 0: (100 + 20 s0) 0 rmoveto, (200 - 40 s0) hlineto, 300 vlineto
            cat(&[blend1(100, 20), i16b(0), vec![RMOVETO], blend1(200, -40), vec![HLINETO], i16b(300), vec![VLINETO]]),
            // 1: font dict 1 defaults to set 1 (two regions): 10 + 8 s0 + 20 s1
            cat(&[i16b(10), i16b(8), i16b(20), i16b(1), vec![BLEND], i16b(0), vec![RMOVETO]]),
            // 2: also font dict 1, but vsindex 0 switches to set 0
            cat(&[i16b(0), vec![VSINDEX], blend1(100, 20), i16b(0), vec![RMOVETO]]),
            // 3: a global subr that blends
            cat(&[i16b(-107), vec![CALLGSUBR], i16b(0), vec![RMOVETO]]),
            // 4: a local subr that blends
            cat(&[i16b(-107), vec![CALLSUBR], i16b(0), vec![RMOVETO]]),
            // 5: blended stems, an implicit vstem before hintmask, then the mask byte must be skipped
            cat(&[i16b(10), i16b(20), i16b(4), i16b(6), i16b(2), vec![BLEND, HSTEMHM], i16b(0), i16b(30), vec![HINTMASK, 0xc0], i16b(1), i16b(1), vec![RMOVETO]]),
            // 6: blend without enough operands
            cat(&[i16b(1), i16b(1), vec![BLEND]]),
            // 7: vsindex out of range
            cat(&[i16b(9), vec![VSINDEX], i16b(1), i16b(1), i16b(1), vec![BLEND]]),
        ];
        let fds = vec![0, 1, 1, 0, 0, 0, 0, 0];
        (g, fds)
    }

    fn cff2(select_format: u8) -> Vec<u8> {
        let (glyphs, fds) = glyphs_v2();
        let gsubrs = index32(&[cat(&[i16b(100), i16b(20), i16b(1), vec![BLEND, RETURN]])]);
        let charstrings = index32(&glyphs);
        let fdsel = fd_select(select_format, &fds);
        let store = var_store();
        let mut vstore = (store.len() as u16).to_be_bytes().to_vec();
        vstore.extend(&store);

        let lsubrs = index32(&[cat(&[i16b(5), i16b(2), i16b(1), vec![BLEND, RETURN]])]);
        // Private DICTs: `29 <int32> op`, six bytes each
        let private0 = [vec![29], 6i32.to_be_bytes().to_vec(), vec![19]].concat(); // Subrs follow right after
        let private1 = [vec![29], 1i32.to_be_bytes().to_vec(), vec![22]].concat(); // vsindex 1

        let top_len = 6 + 7 + 7 + 6;
        let mut pos = 5 + top_len;
        pos += gsubrs.len();
        let cs_at = pos;
        pos += charstrings.len();
        let fdsel_at = pos;
        pos += fdsel.len();
        let fda_at = pos;
        // font dicts: `29 size 29 offset 18` = 11 bytes each, two of them
        let fda_len = index32(&[vec![0; 11], vec![0; 11]]).len();
        pos += fda_len;
        let vstore_at = pos;
        pos += vstore.len();
        let p0_at = pos;
        pos += private0.len() + lsubrs.len();
        let p1_at = pos;
        let fd = |size: usize, off: usize| [vec![29], (size as i32).to_be_bytes().to_vec(), vec![29], (off as i32).to_be_bytes().to_vec(), vec![18]].concat();
        let fda = index32(&[fd(private0.len(), p0_at), fd(private1.len(), p1_at)]);
        assert_eq!(fda.len(), fda_len);

        let int = |v: usize| [vec![29], (v as i32).to_be_bytes().to_vec()].concat();
        let top = cat(&[int(cs_at), vec![17], int(fda_at), vec![12, 36], int(fdsel_at), vec![12, 37], int(vstore_at), vec![24]]);
        assert_eq!(top.len(), top_len);

        let mut d = vec![2, 0, 5];
        d.extend((top_len as u16).to_be_bytes());
        d.extend(top);
        d.extend(gsubrs);
        d.extend(charstrings);
        d.extend(fdsel);
        d.extend(fda);
        d.extend(vstore);
        d.extend(&private0);
        d.extend(lsubrs);
        d.extend(&private1);
        assert_eq!(d.len(), p1_at + private1.len());
        d
    }

    fn v2_glyph(cff: &Cff, d: &[u8], gid: usize) -> Option<Vec<Cmd>> {
        cff.glyph(d, gid).map(|p| p.cmds)
    }

    #[test]
    fn cff2_blend_vsindex_subrs_and_hints() {
        for fmt in [0u8, 3, 4] {
            let d = cff2(fmt);
            let mut cff = Cff::parse(&d).unwrap_or_else(|| panic!("format {fmt}"));
            assert!(cff.is_variable());
            assert_eq!(cff.num_glyphs(), 8);
            cff.set_coords(&[0.5]); // region 0 counts 0.5, region 1 counts 1.0
            let at = |gid| v2_glyph(&cff, &d, gid);
            assert_eq!(
                at(0).unwrap(),
                vec![Cmd::Move(110.0, 0.0), Cmd::Line(290.0, 0.0), Cmd::Line(290.0, 300.0), Cmd::Close],
                "format {fmt}"
            );
            assert_eq!(at(1).unwrap()[0], Cmd::Move(34.0, 0.0), "font dict 1 starts on delta set 1 (format {fmt})");
            assert_eq!(at(2).unwrap()[0], Cmd::Move(110.0, 0.0), "vsindex switches to set 0");
            assert_eq!(at(3).unwrap()[0], Cmd::Move(110.0, 0.0), "blend inside a global subr");
            assert_eq!(at(4).unwrap()[0], Cmd::Move(6.0, 0.0), "blend inside a local subr");
            assert_eq!(at(5).unwrap(), vec![Cmd::Move(1.0, 1.0), Cmd::Close], "the hint mask byte is skipped");
            assert!(at(6).is_none(), "blend needs its operands");
            assert!(at(7).is_none(), "vsindex past the last set");
            assert!(at(8).is_none());

            // other coordinates scale the deltas
            cff.set_coords(&[1.0]);
            assert_eq!(v2_glyph(&cff, &d, 0).unwrap()[0], Cmd::Move(120.0, 0.0));
            cff.set_coords(&[0.0]);
            assert_eq!(v2_glyph(&cff, &d, 0).unwrap()[0], Cmd::Move(100.0, 0.0), "the default instance");
            assert_eq!(v2_glyph(&cff, &d, 1).unwrap()[0], Cmd::Move(10.0, 0.0));
            cff.set_coords(&[]);
            assert_eq!(v2_glyph(&cff, &d, 0).unwrap()[0], Cmd::Move(100.0, 0.0), "missing coordinates count as 0");
        }
    }

    #[test]
    fn cff2_without_a_variation_store_is_a_static_font() {
        // the same structure minus the store: plain charstrings still work, blend does not
        let (glyphs, _) = glyphs_v2();
        let charstrings = index32(&[cat(&[i16b(7), i16b(9), vec![RMOVETO], i16b(5), vec![HLINETO]]), glyphs[6].clone()]);
        let fda_len = index32(&[vec![0; 11]]).len();
        let top_len = 6 + 7;
        let cs_at = 5 + top_len + 4; // header, top dict, empty global subr index
        let fda_at = cs_at + charstrings.len();
        let private = [vec![29], 0i32.to_be_bytes().to_vec(), vec![22]].concat();
        let p_at = fda_at + fda_len;
        let fd = [vec![29], (private.len() as i32).to_be_bytes().to_vec(), vec![29], (p_at as i32).to_be_bytes().to_vec(), vec![18]].concat();
        let fda = index32(&[fd]);
        let int = |v: usize| [vec![29], (v as i32).to_be_bytes().to_vec()].concat();
        let mut d = vec![2, 0, 5];
        d.extend((top_len as u16).to_be_bytes());
        d.extend(cat(&[int(cs_at), vec![17], int(fda_at), vec![12, 36]]));
        d.extend(index32(&[]));
        d.extend(charstrings);
        d.extend(fda);
        d.extend(private);
        let cff = Cff::parse(&d).unwrap();
        assert!(!cff.is_variable());
        assert_eq!(v2_glyph(&cff, &d, 0).unwrap(), vec![Cmd::Move(7.0, 9.0), Cmd::Line(12.0, 9.0), Cmd::Close]);
        assert!(v2_glyph(&cff, &d, 1).is_none());
    }

    #[test]
    fn cff2_damage_does_not_panic_or_hang() {
        let d = cff2(3);
        for cut in 0..d.len() {
            if let Some(mut c) = Cff::parse(&d[..cut]) {
                c.set_coords(&[0.5]);
                for g in 0..9 {
                    let _ = c.glyph(&d[..cut], g);
                }
            }
        }
        for i in 0..d.len() {
            let mut b = d.clone();
            b[i] ^= 0xff;
            if let Some(mut c) = Cff::parse(&b) {
                c.set_coords(&[0.5]);
                for g in 0..9 {
                    let _ = c.glyph(&b, g);
                }
            }
        }
    }
}
