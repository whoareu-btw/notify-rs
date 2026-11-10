//! JPEG decoder: baseline, extended-sequential and progressive Huffman JPEG,
//! 8-bit, grey / YCbCr / RGB / CMYK / YCCK, restart intervals. Uses the same
//! integer IDCT, fixed-point colour conversion and "fancy" chroma upsampling
//! as libjpeg, so results match what browsers and PIL show.
//! EXIF orientation is not applied.

use super::Image;

const MAX_PIXELS: usize = 25_000_000;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50,
    43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub fn is_jpeg(d: &[u8]) -> bool {
    d.len() > 3 && d[0] == 0xff && d[1] == 0xd8 && d[2] == 0xff
}

// ---------------------------------------------------------------- Huffman

struct Huff {
    fast: [u16; 512],
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    vals: Vec<u8>,
}

impl Huff {
    fn new(counts: &[u8; 16], vals: Vec<u8>) -> Result<Huff, String> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total > 256 || vals.len() < total {
            return Err("bad Huffman table".into());
        }
        let mut h = Huff { fast: [0; 512], maxcode: [-1; 18], valptr: [0; 17], mincode: [0; 17], vals };
        let mut code = 0i32;
        let mut k = 0i32;
        for len in 1..=16usize {
            let n = counts[len - 1] as i32;
            h.valptr[len] = k;
            h.mincode[len] = code;
            if n > 0 {
                if code + n > (1 << len) {
                    return Err("over-subscribed Huffman table".into());
                }
                if len <= 9 {
                    for i in 0..n {
                        let c = (code + i) as usize;
                        let sym = h.vals[(k + i) as usize] as u16;
                        let shift = 9 - len;
                        for fill in 0..(1usize << shift) {
                            h.fast[(c << shift) | fill] = ((len as u16) << 8) | sym;
                        }
                    }
                }
                h.maxcode[len] = code + n - 1;
            }
            code = (code + n) << 1;
            k += n;
        }
        h.maxcode[17] = i32::MAX;
        Ok(h)
    }
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
    /// zero bits appended after the data ended or a marker was reached
    pad: u32,
    marker: bool,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8], pos: usize) -> Self {
        Bits { d, pos, acc: 0, n: 0, pad: 0, marker: false }
    }

    fn refill(&mut self) {
        while self.n <= 48 {
            let mut byte = 0u8;
            let mut real = false;
            if !self.marker && self.pos < self.d.len() {
                let b = self.d[self.pos];
                if b == 0xff {
                    match self.d.get(self.pos + 1) {
                        Some(0) => {
                            byte = 0xff;
                            real = true;
                            self.pos += 2;
                        }
                        Some(0xff) => {
                            self.pos += 1; // fill byte: look again
                            continue;
                        }
                        _ => self.marker = true, // a real marker (or EOF after FF)
                    }
                } else {
                    byte = b;
                    real = true;
                    self.pos += 1;
                }
            } else {
                self.marker = true;
            }
            if !real {
                self.pad += 8;
            }
            self.acc |= (byte as u64) << (56 - self.n);
            self.n += 8;
        }
    }

    /// More bits were consumed than the data held.
    fn exhausted(&self) -> bool {
        self.pad > self.n
    }

    #[inline]
    fn peek9(&mut self) -> usize {
        if self.n < 16 {
            self.refill();
        }
        (self.acc >> 55) as usize
    }

    #[inline]
    fn skip(&mut self, k: u32) {
        self.acc <<= k;
        self.n -= k;
    }

    #[inline]
    fn bits(&mut self, k: u32) -> i32 {
        if k == 0 {
            return 0;
        }
        if self.n < k {
            self.refill();
        }
        let v = (self.acc >> (64 - k)) as i32;
        self.skip(k);
        v
    }

    #[inline]
    fn bit(&mut self) -> bool {
        self.bits(1) != 0
    }

    fn decode(&mut self, h: &Huff) -> Result<u8, String> {
        let e = h.fast[self.peek9()];
        if e != 0 {
            self.skip((e >> 8) as u32);
            return Ok(e as u8);
        }
        // slow path: lengths 10..=16
        if self.n < 16 {
            self.refill();
        }
        let mut code = (self.acc >> 55) as i32; // 9 bits
        for len in 10..=16usize {
            code = (code << 1) | ((self.acc >> (64 - len)) & 1) as i32;
            if h.maxcode[len] >= 0 && code <= h.maxcode[len] && code >= h.mincode[len] {
                self.skip(len as u32);
                let i = h.valptr[len] + code - h.mincode[len];
                return h.vals.get(i as usize).copied().ok_or_else(|| "bad Huffman code".to_string());
            }
        }
        Err("bad Huffman code".into())
    }

    /// Discard buffered bits and step over the next RSTn marker.
    fn restart(&mut self) {
        self.acc = 0;
        self.n = 0;
        self.pad = 0;
        // find the marker at or after pos
        while self.pos + 1 < self.d.len() {
            if self.d[self.pos] == 0xff && (0xd0..=0xd7).contains(&self.d[self.pos + 1]) {
                self.pos += 2;
                break;
            }
            if self.d[self.pos] == 0xff && self.d[self.pos + 1] != 0 && self.d[self.pos + 1] != 0xff {
                break; // some other marker: scan is over
            }
            self.pos += 1;
        }
        self.marker = false;
    }
}

#[inline]
fn extend(v: i32, t: u32) -> i32 {
    if t == 0 {
        0
    } else if v < (1 << (t - 1)) {
        v - (1 << t) + 1
    } else {
        v
    }
}

// ---------------------------------------------------------------- frame

struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// blocks in the padded plane
    bw: usize,
    bh: usize,
    /// blocks that cover the real component size (non-interleaved scans use these)
    rbw: usize,
    rbh: usize,
    /// real sample size
    sw: usize,
    sh: usize,
    coefs: Vec<i16>,
    pred: i32,
    dc_tbl: usize,
    ac_tbl: usize,
}

struct Frame {
    w: usize,
    h: usize,
    progressive: bool,
    comps: Vec<Comp>,
    hmax: usize,
    vmax: usize,
    mcus_x: usize,
    mcus_y: usize,
}

pub fn dims(d: &[u8]) -> Option<(u32, u32)> {
    let mut p = 2;
    while p + 4 <= d.len() {
        if d[p] != 0xff {
            p += 1;
            continue;
        }
        let m = d[p + 1];
        if m == 0xff {
            p += 1;
            continue;
        }
        if (0xd0..=0xd8).contains(&m) || m == 0x01 || m == 0 {
            p += 2;
            continue;
        }
        let len = u16::from_be_bytes([d[p + 2], d[p + 3]]) as usize;
        if matches!(m, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let h = u16::from_be_bytes([*d.get(p + 5)?, *d.get(p + 6)?]) as u32;
            let w = u16::from_be_bytes([*d.get(p + 7)?, *d.get(p + 8)?]) as u32;
            return Some((w, h));
        }
        p += 2 + len;
    }
    None
}

pub fn decode(d: &[u8]) -> Result<Image, String> {
    if !is_jpeg(d) {
        return Err("not a JPEG".into());
    }
    let mut qt: [[u16; 64]; 4] = [[1; 64]; 4];
    let mut dc: [Option<Huff>; 4] = [None, None, None, None];
    let mut ac: [Option<Huff>; 4] = [None, None, None, None];
    let mut frame: Option<Frame> = None;
    let mut restart_interval = 0usize;
    let mut adobe: Option<u8> = None;
    let mut scans = 0usize;
    let mut pos = 2usize;

    loop {
        // find the next marker
        while pos < d.len() && d[pos] != 0xff {
            pos += 1;
        }
        while pos < d.len() && d[pos] == 0xff {
            pos += 1;
        }
        if pos >= d.len() {
            break;
        }
        let marker = d[pos];
        pos += 1;
        match marker {
            0x00 | 0x01 | 0xd0..=0xd8 => continue,
            0xd9 => break,
            _ => {}
        }
        if pos + 2 > d.len() {
            break;
        }
        let len = u16::from_be_bytes([d[pos], d[pos + 1]]) as usize;
        if len < 2 || pos + len > d.len() {
            // truncated header: stop and use what we have
            break;
        }
        let seg = &d[pos + 2..pos + len];
        let seg_end = pos + len;
        match marker {
            0xc0..=0xc2 => {
                if frame.is_some() {
                    return Err("multiple frames".into());
                }
                frame = Some(parse_sof(seg, marker == 0xc2)?);
            }
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return Err("unsupported JPEG type (lossless or arithmetic coding)".into()),
            0xc4 => {
                let mut s = seg;
                while s.len() >= 17 {
                    let (class, id) = (s[0] >> 4, (s[0] & 15) as usize);
                    let mut counts = [0u8; 16];
                    counts.copy_from_slice(&s[1..17]);
                    let n: usize = counts.iter().map(|&c| c as usize).sum();
                    if s.len() < 17 + n || id > 3 || class > 1 {
                        return Err("bad DHT".into());
                    }
                    let t = Huff::new(&counts, s[17..17 + n].to_vec())?;
                    if class == 0 {
                        dc[id] = Some(t);
                    } else {
                        ac[id] = Some(t);
                    }
                    s = &s[17 + n..];
                }
            }
            0xdb => {
                let mut s = seg;
                while !s.is_empty() {
                    let (prec, id) = (s[0] >> 4, (s[0] & 15) as usize);
                    let sz = if prec == 0 { 64 } else { 128 };
                    if id > 3 || s.len() < 1 + sz {
                        return Err("bad DQT".into());
                    }
                    for i in 0..64 {
                        qt[id][i] = if prec == 0 { s[1 + i] as u16 } else { u16::from_be_bytes([s[1 + 2 * i], s[2 + 2 * i]]) };
                    }
                    s = &s[1 + sz..];
                }
            }
            0xdd => {
                if seg.len() >= 2 {
                    restart_interval = u16::from_be_bytes([seg[0], seg[1]]) as usize;
                }
            }
            0xee => {
                if seg.len() >= 12 && &seg[..5] == b"Adobe" {
                    adobe = Some(seg[11]);
                }
            }
            0xda => {
                let f = frame.as_mut().ok_or("scan before frame header")?;
                let hdr = parse_sos(seg, f)?;
                let mut bits = Bits::new(d, seg_end);
                let ok = decode_scan(f, &hdr, &mut bits, &dc, &ac, restart_interval)?;
                scans += 1;
                pos = bits.pos;
                if !ok {
                    break; // ran out of data: keep what we have
                }
                continue;
            }
            _ => {}
        }
        pos = seg_end;
    }

    let f = frame.ok_or("no frame header")?;
    if scans == 0 {
        return Err("no image data".into());
    }
    Ok(finish(&f, &qt, adobe))
}

fn parse_sof(seg: &[u8], progressive: bool) -> Result<Frame, String> {
    if seg.len() < 6 {
        return Err("bad SOF".into());
    }
    if seg[0] != 8 {
        return Err("only 8-bit JPEG is supported".into());
    }
    let h = u16::from_be_bytes([seg[1], seg[2]]) as usize;
    let w = u16::from_be_bytes([seg[3], seg[4]]) as usize;
    let n = seg[5] as usize;
    if w == 0 || h == 0 || w * h > MAX_PIXELS {
        return Err("image size not supported".into());
    }
    if !(n == 1 || n == 3 || n == 4) || seg.len() < 6 + 3 * n {
        return Err("unsupported component count".into());
    }
    let mut comps = Vec::new();
    for i in 0..n {
        let c = &seg[6 + 3 * i..9 + 3 * i];
        let (hs, vs) = ((c[1] >> 4) as usize, (c[1] & 15) as usize);
        if !(1..=4).contains(&hs) || !(1..=4).contains(&vs) || c[2] > 3 {
            return Err("bad sampling factors".into());
        }
        comps.push(Comp {
            id: c[0],
            h: hs,
            v: vs,
            tq: c[2] as usize,
            bw: 0,
            bh: 0,
            rbw: 0,
            rbh: 0,
            sw: 0,
            sh: 0,
            coefs: Vec::new(),
            pred: 0,
            dc_tbl: 0,
            ac_tbl: 0,
        });
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap();
    let vmax = comps.iter().map(|c| c.v).max().unwrap();
    let mcus_x = w.div_ceil(8 * hmax);
    let mcus_y = h.div_ceil(8 * vmax);
    let coef_total: usize = comps.iter().map(|c| mcus_x * c.h * mcus_y * c.v * 64).sum();
    if coef_total > 160_000_000 {
        return Err("image too large".into());
    }
    for c in &mut comps {
        c.sw = (w * c.h).div_ceil(hmax);
        c.sh = (h * c.v).div_ceil(vmax);
        c.rbw = c.sw.div_ceil(8);
        c.rbh = c.sh.div_ceil(8);
        c.bw = mcus_x * c.h;
        c.bh = mcus_y * c.v;
        c.coefs = vec![0i16; c.bw * c.bh * 64];
    }
    Ok(Frame { w, h, progressive, comps, hmax, vmax, mcus_x, mcus_y })
}

struct ScanHdr {
    comps: Vec<usize>,
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

fn parse_sos(seg: &[u8], f: &mut Frame) -> Result<ScanHdr, String> {
    let n = *seg.first().ok_or("bad SOS")? as usize;
    if n == 0 || n > 4 || seg.len() < 1 + 2 * n + 3 {
        return Err("bad SOS".into());
    }
    let mut comps = Vec::new();
    for i in 0..n {
        let (cid, tbl) = (seg[1 + 2 * i], seg[2 + 2 * i]);
        let idx = f.comps.iter().position(|c| c.id == cid).ok_or("scan names an unknown component")?;
        f.comps[idx].dc_tbl = (tbl >> 4) as usize & 3;
        f.comps[idx].ac_tbl = (tbl & 15) as usize & 3;
        comps.push(idx);
    }
    let t = &seg[1 + 2 * n..];
    let (ss, se, ah, al) = (t[0] as usize, t[1] as usize, (t[2] >> 4) as u32, (t[2] & 15) as u32);
    if f.progressive {
        if ss > se || se > 63 || (ss == 0 && se != 0) || (ss > 0 && n != 1) || al > 13 {
            return Err("bad progressive scan parameters".into());
        }
    }
    Ok(ScanHdr { comps, ss: if f.progressive { ss } else { 0 }, se: if f.progressive { se } else { 63 }, ah, al })
}

/// Returns Ok(false) if the data ended early.
fn decode_scan(
    f: &mut Frame,
    s: &ScanHdr,
    bits: &mut Bits,
    dc: &[Option<Huff>; 4],
    ac: &[Option<Huff>; 4],
    restart_interval: usize,
) -> Result<bool, String> {
    for &ci in &s.comps {
        f.comps[ci].pred = 0;
    }
    let need_dc = !f.progressive || (s.ss == 0 && s.ah == 0);
    let need_ac = !f.progressive || s.ss > 0;
    for &ci in &s.comps {
        let c = &f.comps[ci];
        if need_dc && dc[c.dc_tbl].is_none() {
            return Err("missing DC Huffman table".into());
        }
        if need_ac && ac[c.ac_tbl].is_none() {
            return Err("missing AC Huffman table".into());
        }
    }
    let mut eobrun = 0u32;
    let interleaved = s.comps.len() > 1;
    let (mcu_w, mcu_h) = if interleaved {
        (f.mcus_x, f.mcus_y)
    } else {
        (f.comps[s.comps[0]].rbw, f.comps[s.comps[0]].rbh)
    };
    let total = mcu_w * mcu_h;
    let mut count = 0usize;
    for my in 0..mcu_h {
        for mx in 0..mcu_w {
            if restart_interval > 0 && count > 0 && count % restart_interval == 0 {
                bits.restart();
                for &ci in &s.comps {
                    f.comps[ci].pred = 0;
                }
                eobrun = 0;
            }
            count += 1;
            for &ci in &s.comps {
                let (ch, cv) = if interleaved { (f.comps[ci].h, f.comps[ci].v) } else { (1, 1) };
                for v in 0..cv {
                    for h in 0..ch {
                        let (bx, by) = (mx * ch + h, my * cv + v);
                        let comp = &mut f.comps[ci];
                        let off = (by * comp.bw + bx) * 64;
                        let block: &mut [i16] = &mut comp.coefs[off..off + 64];
                        let dct = dc[comp.dc_tbl].as_ref();
                        let act = ac[comp.ac_tbl].as_ref();
                        if !f.progressive {
                            decode_baseline(bits, block, &mut comp.pred, dct.unwrap(), act.unwrap())?;
                        } else if s.ss == 0 {
                            if s.ah == 0 {
                                let t = bits.decode(dct.unwrap())? as u32;
                                if t > 15 {
                                    return Err("bad DC code".into());
                                }
                                let diff = extend(bits.bits(t), t);
                                comp.pred = comp.pred.wrapping_add(diff);
                                block[0] = (comp.pred << s.al) as i16;
                            } else if bits.bit() {
                                block[0] |= (1 << s.al) as i16;
                            }
                        } else if s.ah == 0 {
                            decode_ac_first(bits, block, act.unwrap(), s, &mut eobrun)?;
                        } else {
                            decode_ac_refine(bits, block, act.unwrap(), s, &mut eobrun)?;
                        }
                    }
                }
            }
            if bits.exhausted() {
                return Ok(false);
            }
        }
    }
    let _ = total;
    Ok(true)
}

fn decode_baseline(b: &mut Bits, block: &mut [i16], pred: &mut i32, dct: &Huff, act: &Huff) -> Result<(), String> {
    let t = b.decode(dct)? as u32;
    if t > 15 {
        return Err("bad DC code".into());
    }
    *pred = pred.wrapping_add(extend(b.bits(t), t));
    block[0] = *pred as i16;
    let mut k = 1;
    while k < 64 {
        let rs = b.decode(act)?;
        let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
        if s == 0 {
            if r == 15 {
                k += 16;
                continue;
            }
            break;
        }
        k += r;
        if k > 63 {
            return Err("coefficient index out of range".into());
        }
        block[ZIGZAG[k]] = extend(b.bits(s), s) as i16;
        k += 1;
    }
    Ok(())
}

fn decode_ac_first(b: &mut Bits, block: &mut [i16], act: &Huff, s: &ScanHdr, eobrun: &mut u32) -> Result<(), String> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = s.ss;
    while k <= s.se {
        let rs = b.decode(act)?;
        let (r, sz) = ((rs >> 4) as usize, (rs & 15) as u32);
        if sz == 0 {
            if r < 15 {
                *eobrun = (1 << r) - 1;
                if r > 0 {
                    *eobrun += b.bits(r as u32) as u32;
                }
                break;
            }
            k += 16;
            continue;
        }
        k += r;
        if k > 63 {
            return Err("coefficient index out of range".into());
        }
        block[ZIGZAG[k]] = (extend(b.bits(sz), sz) << s.al) as i16;
        k += 1;
    }
    Ok(())
}

fn decode_ac_refine(b: &mut Bits, block: &mut [i16], act: &Huff, s: &ScanHdr, eobrun: &mut u32) -> Result<(), String> {
    let p1: i16 = 1 << s.al;
    let m1: i16 = -1 << s.al;
    let mut k = s.ss;
    let refine = |b: &mut Bits, c: &mut i16| {
        if *c != 0 && b.bit() && (*c & p1) == 0 {
            *c = c.wrapping_add(if *c >= 0 { p1 } else { m1 });
        }
    };
    if *eobrun == 0 {
        while k <= s.se {
            let rs = b.decode(act)?;
            let (mut r, sz) = ((rs >> 4) as i32, (rs & 15) as u32);
            let mut val: i16 = 0;
            if sz != 0 {
                val = if b.bit() { p1 } else { m1 };
            } else if r != 15 {
                *eobrun = 1 << r;
                if r > 0 {
                    *eobrun += b.bits(r as u32) as u32;
                }
                break;
            }
            while k <= s.se {
                let z = ZIGZAG[k];
                if block[z] != 0 {
                    refine(b, &mut block[z]);
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
            }
            if val != 0 && k <= 63 {
                block[ZIGZAG[k]] = val;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        while k <= s.se {
            let z = ZIGZAG[k];
            if block[z] != 0 {
                refine(b, &mut block[z]);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

// ---------------------------------------------------------------- IDCT

const CONST_BITS: i64 = 13;
const PASS1_BITS: i64 = 2;
const F_0_298: i64 = 2446;
const F_0_390: i64 = 3196;
const F_0_541: i64 = 4433;
const F_0_765: i64 = 6270;
const F_0_899: i64 = 7373;
const F_1_175: i64 = 9633;
const F_1_501: i64 = 12299;
const F_1_847: i64 = 15137;
const F_1_961: i64 = 16069;
const F_2_053: i64 = 16819;
const F_2_562: i64 = 20995;
const F_3_072: i64 = 25172;

#[inline]
fn descale(x: i64, n: i64) -> i64 {
    (x + (1 << (n - 1))) >> n
}

/// libjpeg's "islow" inverse DCT on dequantised coefficients (natural order).
fn idct(inp: &[i32; 64], out: &mut [u8; 64]) {
    // 64-bit intermediates: identical results for real images, and no overflow
    // (a debug-build panic) on corrupt coefficients.
    let mut ws = [0i64; 64];
    for col in 0..8 {
        let g = |r: usize| inp[r * 8 + col] as i64;
        if (1..8).all(|r| g(r) == 0) {
            let dc = g(0) << PASS1_BITS;
            for r in 0..8 {
                ws[r * 8 + col] = dc;
            }
            continue;
        }
        let (z2, z3) = (g(2), g(6));
        let z1 = (z2 + z3) * F_0_541;
        let tmp2 = z1 - z3 * F_1_847;
        let tmp3 = z1 + z2 * F_0_765;
        let (z2, z3) = (g(0), g(4));
        let tmp0 = (z2 + z3) << CONST_BITS;
        let tmp1 = (z2 - z3) << CONST_BITS;
        let (t10, t13, t11, t12) = (tmp0 + tmp3, tmp0 - tmp3, tmp1 + tmp2, tmp1 - tmp2);
        let (mut t0, mut t1, mut t2, mut t3) = (g(7), g(5), g(3), g(1));
        let (z1, z2, z3, z4) = (t0 + t3, t1 + t2, t0 + t2, t1 + t3);
        let z5 = (z3 + z4) * F_1_175;
        t0 *= F_0_298;
        t1 *= F_2_053;
        t2 *= F_3_072;
        t3 *= F_1_501;
        let (z1, z2) = (-z1 * F_0_899, -z2 * F_2_562);
        let (z3, z4) = (-z3 * F_1_961 + z5, -z4 * F_0_390 + z5);
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;
        let n = CONST_BITS - PASS1_BITS;
        ws[col] = descale(t10 + t3, n);
        ws[56 + col] = descale(t10 - t3, n);
        ws[8 + col] = descale(t11 + t2, n);
        ws[48 + col] = descale(t11 - t2, n);
        ws[16 + col] = descale(t12 + t1, n);
        ws[40 + col] = descale(t12 - t1, n);
        ws[24 + col] = descale(t13 + t0, n);
        ws[32 + col] = descale(t13 - t0, n);
    }
    for row in 0..8 {
        let w = &ws[row * 8..row * 8 + 8];
        let o = &mut out[row * 8..row * 8 + 8];
        let n = CONST_BITS + PASS1_BITS + 3;
        let put = |v: i64| (v + 128).clamp(0, 255) as u8;
        if w[1..].iter().all(|&v| v == 0) {
            let dc = put(descale(w[0], PASS1_BITS + 3));
            o.fill(dc);
            continue;
        }
        let (z2, z3) = (w[2], w[6]);
        let z1 = (z2 + z3) * F_0_541;
        let tmp2 = z1 - z3 * F_1_847;
        let tmp3 = z1 + z2 * F_0_765;
        let tmp0 = (w[0] + w[4]) << CONST_BITS;
        let tmp1 = (w[0] - w[4]) << CONST_BITS;
        let (t10, t13, t11, t12) = (tmp0 + tmp3, tmp0 - tmp3, tmp1 + tmp2, tmp1 - tmp2);
        let (mut t0, mut t1, mut t2, mut t3) = (w[7], w[5], w[3], w[1]);
        let (z1, z2, z3, z4) = (t0 + t3, t1 + t2, t0 + t2, t1 + t3);
        let z5 = (z3 + z4) * F_1_175;
        t0 *= F_0_298;
        t1 *= F_2_053;
        t2 *= F_3_072;
        t3 *= F_1_501;
        let (z1, z2) = (-z1 * F_0_899, -z2 * F_2_562);
        let (z3, z4) = (-z3 * F_1_961 + z5, -z4 * F_0_390 + z5);
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;
        o[0] = put(descale(t10 + t3, n));
        o[7] = put(descale(t10 - t3, n));
        o[1] = put(descale(t11 + t2, n));
        o[6] = put(descale(t11 - t2, n));
        o[2] = put(descale(t12 + t1, n));
        o[5] = put(descale(t12 - t1, n));
        o[3] = put(descale(t13 + t0, n));
        o[4] = put(descale(t13 - t0, n));
    }
}

// ---------------------------------------------------------------- output

/// A decoded component plane (real size sw x sh).
struct Plane {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

fn component_plane(c: &Comp, q: &[u16; 64]) -> Plane {
    let stride = c.bw * 8;
    let mut px = vec![0u8; stride * c.bh * 8];
    // dequantisation table in natural order
    let mut qn = [0i32; 64];
    for k in 0..64 {
        qn[ZIGZAG[k]] = q[k] as i32;
    }
    let mut inp = [0i32; 64];
    let mut out = [0u8; 64];
    for by in 0..c.bh {
        for bx in 0..c.bw {
            let b = &c.coefs[(by * c.bw + bx) * 64..][..64];
            for i in 0..64 {
                inp[i] = b[i] as i32 * qn[i];
            }
            idct(&inp, &mut out);
            for r in 0..8 {
                let dst = (by * 8 + r) * stride + bx * 8;
                px[dst..dst + 8].copy_from_slice(&out[r * 8..r * 8 + 8]);
            }
        }
    }
    // crop to the real size
    let mut crop = Vec::with_capacity(c.sw * c.sh);
    for y in 0..c.sh {
        crop.extend_from_slice(&px[y * stride..y * stride + c.sw]);
    }
    Plane { w: c.sw, h: c.sh, px: crop }
}

/// Upsample to the full image size the way libjpeg does.
fn upsample(p: &Plane, hr: usize, vr: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let at = |x: isize, y: isize| -> i32 {
        let x = x.clamp(0, p.w as isize - 1) as usize;
        let y = y.clamp(0, p.h as isize - 1) as usize;
        p.px[y * p.w + x] as i32
    };
    match (hr, vr) {
        (1, 1) => {
            for y in 0..h {
                out[y * w..y * w + w].copy_from_slice(&p.px[y * p.w..y * p.w + w]);
            }
        }
        (2, 1) if p.w > 2 => {
            for y in 0..h {
                for x in 0..w {
                    let i = (x / 2) as isize;
                    let c = at(i, y as isize);
                    out[y * w + x] = if x % 2 == 0 { ((3 * c + at(i - 1, y as isize) + 1) >> 2) as u8 } else { ((3 * c + at(i + 1, y as isize) + 2) >> 2) as u8 };
                }
            }
        }
        (2, 2) if p.w > 2 => {
            for y in 0..h {
                let sy = (y / 2) as isize;
                let oy = if y % 2 == 0 { sy - 1 } else { sy + 1 };
                let colsum = |x: isize| 3 * at(x, sy) + at(x, oy);
                for x in 0..w {
                    let i = (x / 2) as isize;
                    let this = colsum(i);
                    out[y * w + x] = if x % 2 == 0 { ((3 * this + colsum(i - 1) + 8) >> 4) as u8 } else { ((3 * this + colsum(i + 1) + 7) >> 4) as u8 };
                }
            }
        }
        (1, 2) => {
            for y in 0..h {
                let sy = (y / 2) as isize;
                for x in 0..w {
                    let c = at(x as isize, sy);
                    out[y * w + x] = if y % 2 == 0 { ((3 * c + at(x as isize, sy - 1) + 1) >> 2) as u8 } else { ((3 * c + at(x as isize, sy + 1) + 2) >> 2) as u8 };
                }
            }
        }
        _ => {
            // other ratios: replicate samples
            for y in 0..h {
                for x in 0..w {
                    out[y * w + x] = at((x / hr) as isize, (y / vr) as isize) as u8;
                }
            }
        }
    }
    out
}

fn finish(f: &Frame, qt: &[[u16; 64]; 4], adobe: Option<u8>) -> Image {
    let (w, h) = (f.w, f.h);
    let planes: Vec<Vec<u8>> = f
        .comps
        .iter()
        .map(|c| {
            let p = component_plane(c, &qt[c.tq]);
            upsample(&p, f.hmax / c.h.min(f.hmax), f.vmax / c.v.min(f.vmax), w, h)
        })
        .collect();

    // libjpeg's fixed-point YCbCr -> RGB tables
    let fix = |x: f64| (x * 65536.0 + 0.5) as i32;
    let (cr_r, cb_b, cr_g, cb_g): (Vec<i32>, Vec<i32>, Vec<i32>, Vec<i32>) = {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut c = Vec::new();
        let mut e = Vec::new();
        for i in 0..256i32 {
            let x = i - 128;
            a.push((fix(1.402) * x + 32768) >> 16);
            b.push((fix(1.772) * x + 32768) >> 16);
            c.push(-fix(0.71414) * x);
            e.push(-fix(0.34414) * x + 32768);
        }
        (a, b, c, e)
    };
    let ycc = |y: u8, cb: u8, cr: u8| -> [u8; 3] {
        let (y, cb, cr) = (y as i32, cb as usize, cr as usize);
        [
            (y + cr_r[cr]).clamp(0, 255) as u8,
            (y + ((cb_g[cb] + cr_g[cr]) >> 16)).clamp(0, 255) as u8,
            (y + cb_b[cb]).clamp(0, 255) as u8,
        ]
    };

    let n = f.comps.len();
    let is_rgb = n == 3 && (adobe == Some(0) || (adobe.is_none() && f.comps.iter().map(|c| c.id).eq([b'R', b'G', b'B'])));
    let mut rgba = vec![255u8; w * h * 4];
    for i in 0..w * h {
        let px = match n {
            1 => {
                let g = planes[0][i];
                [g, g, g]
            }
            3 if is_rgb => [planes[0][i], planes[1][i], planes[2][i]],
            3 => ycc(planes[0][i], planes[1][i], planes[2][i]),
            _ => {
                // 4 components: Adobe CMYK/YCCK files store inverted values
                let (c, m, y, k) = if adobe == Some(2) {
                    let [r, g, b] = ycc(planes[0][i], planes[1][i], planes[2][i]);
                    (255 - r, 255 - g, 255 - b, planes[3][i])
                } else {
                    (planes[0][i], planes[1][i], planes[2][i], planes[3][i])
                };
                let mul = |a: u8, b: u8| ((a as u32 * b as u32 + 127) / 255) as u8;
                if adobe.is_some() {
                    [mul(c, k), mul(m, k), mul(y, k)]
                } else {
                    [mul(255 - c, 255 - k), mul(255 - m, 255 - k), mul(255 - y, 255 - k)]
                }
            }
        };
        rgba[i * 4..i * 4 + 3].copy_from_slice(&px);
    }
    Image { w, h, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extend_matches_the_spec() {
        assert_eq!(extend(0, 1), -1);
        assert_eq!(extend(1, 1), 1);
        assert_eq!(extend(0b011, 3), -4);
        assert_eq!(extend(0b100, 3), 4);
        assert_eq!(extend(0, 0), 0);
    }

    #[test]
    fn idct_of_a_dc_only_block_is_flat() {
        let mut inp = [0i32; 64];
        inp[0] = 8 * 40; // DC 40 after dequantisation
        let mut out = [0u8; 64];
        idct(&inp, &mut out);
        assert!(out.iter().all(|&v| v == out[0]), "{out:?}");
        assert_eq!(out[0] as i32, 128 + 40);
    }

    #[test]
    fn idct_matches_the_float_definition() {
        // a few arbitrary blocks against the textbook formula
        let mut s: u32 = 3;
        for _ in 0..50 {
            let mut inp = [0i32; 64];
            for v in inp.iter_mut().take(20) {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = ((s >> 20) as i32 % 200) - 100;
            }
            let mut out = [0u8; 64];
            idct(&inp, &mut out);
            for y in 0..8 {
                for x in 0..8 {
                    let mut sum = 0.0f64;
                    for v in 0..8 {
                        for u in 0..8 {
                            let cu = if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
                            let cv = if v == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
                            sum += cu * cv * inp[v * 8 + u] as f64
                                * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos()
                                * ((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI / 16.0).cos();
                        }
                    }
                    let want = (sum / 4.0 + 128.0).round().clamp(0.0, 255.0);
                    assert!((out[y * 8 + x] as f64 - want).abs() <= 1.0, "({x},{y}) {} vs {want}", out[y * 8 + x]);
                }
            }
        }
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xff, 0xd8, 0xff]).is_err());
        let mut s: u32 = 11;
        for _ in 0..300 {
            let mut v = vec![0xff, 0xd8, 0xff];
            for _ in 0..(s % 200) {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                v.push((s >> 24) as u8);
            }
            let _ = decode(&v);
        }
    }
}
