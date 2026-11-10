//! PNG decoder: every colour type and bit depth, tRNS, Adam7 interlacing.
//! Output is straight (non-premultiplied) RGBA8; 16-bit samples keep their high byte.

use super::inflate::zlib_decompress;
use super::Image;

const MAX_PIXELS: usize = 40_000_000;

struct Header {
    w: usize,
    h: usize,
    depth: u8,
    ctype: u8,
    interlace: bool,
}

fn be32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*d.get(o)?, *d.get(o + 1)?, *d.get(o + 2)?, *d.get(o + 3)?]))
}

pub fn is_png(d: &[u8]) -> bool {
    d.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

/// (width, height) without decoding.
pub fn dims(d: &[u8]) -> Option<(u32, u32)> {
    if !is_png(d) || d.get(12..16) != Some(b"IHDR") {
        return None;
    }
    Some((be32(d, 16)?, be32(d, 20)?))
}

fn channels(ctype: u8) -> Option<usize> {
    Some(match ctype {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    })
}

pub fn decode(d: &[u8]) -> Result<Image, String> {
    if !is_png(d) {
        return Err("not a PNG".into());
    }
    let mut pos = 8;
    let mut hdr: Option<Header> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    loop {
        let len = be32(d, pos).ok_or("truncated PNG")? as usize;
        let kind = d.get(pos + 4..pos + 8).ok_or("truncated PNG")?;
        let body = d.get(pos + 8..pos + 8 + len).ok_or("truncated PNG chunk")?;
        match kind {
            b"IHDR" => {
                if len < 13 {
                    return Err("bad IHDR".into());
                }
                let (w, h) = (be32(body, 0).unwrap() as usize, be32(body, 4).unwrap() as usize);
                if w == 0 || h == 0 || w.checked_mul(h).map_or(true, |p| p > MAX_PIXELS) || w > 1 << 20 || h > 1 << 20 {
                    return Err("image size not supported".into());
                }
                let (depth, ctype) = (body[8], body[9]);
                let ok = matches!((ctype, depth), (0, 1 | 2 | 4 | 8 | 16) | (2, 8 | 16) | (3, 1 | 2 | 4 | 8) | (4, 8 | 16) | (6, 8 | 16));
                if !ok {
                    return Err("bad colour type / bit depth".into());
                }
                if body[10] != 0 || body[11] != 0 || body[12] > 1 {
                    return Err("unsupported compression, filter or interlace method".into());
                }
                hdr = Some(Header { w, h, depth, ctype, interlace: body[12] == 1 });
            }
            b"PLTE" => palette = body.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len; // length, type, data, crc
        if pos >= d.len() {
            break;
        }
    }
    let h = hdr.ok_or("missing IHDR")?;
    if h.ctype == 3 && palette.is_empty() {
        return Err("palette image without PLTE".into());
    }

    let ch = channels(h.ctype).ok_or("bad colour type")?;
    let bits_pp = ch * h.depth as usize;
    let bpp = bits_pp.div_ceil(8).max(1);
    let passes: Vec<(usize, usize, usize, usize)> = if h.interlace {
        // (x start, y start, x step, y step)
        vec![(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
    } else {
        vec![(0, 0, 1, 1)]
    };
    let dims_of = |p: &(usize, usize, usize, usize)| ((h.w + p.2 - 1 - p.0) / p.2, (h.h + p.3 - 1 - p.1) / p.3);
    let mut expected = 0usize;
    for p in &passes {
        let (pw, ph) = dims_of(p);
        if pw > 0 && ph > 0 {
            expected += (1 + (pw * bits_pp).div_ceil(8)) * ph;
        }
    }
    let raw = zlib_decompress(&idat, expected + 1024)?;
    if raw.len() < expected {
        return Err("image data is truncated".into());
    }

    let mut rgba = vec![0u8; h.w * h.h * 4];
    let mut off = 0;
    let mut prev: Vec<u8> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for p in &passes {
        let (pw, ph) = dims_of(p);
        if pw == 0 || ph == 0 {
            continue;
        }
        let stride = (pw * bits_pp).div_ceil(8);
        prev.clear();
        prev.resize(stride, 0);
        for y in 0..ph {
            let ft = raw[off];
            cur.clear();
            cur.extend_from_slice(&raw[off + 1..off + 1 + stride]);
            off += 1 + stride;
            unfilter(ft, &mut cur, &prev, bpp)?;
            for x in 0..pw {
                let px = pixel(&cur, x, &h, ch, &palette, &trns);
                let (ox, oy) = (p.0 + x * p.2, p.1 + y * p.3);
                let i = (oy * h.w + ox) * 4;
                rgba[i..i + 4].copy_from_slice(&px);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
    }
    Ok(Image { w: h.w, h: h.h, rgba })
}

fn unfilter(ft: u8, cur: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), String> {
    match ft {
        0 => {}
        1 => {
            for i in bpp..cur.len() {
                cur[i] = cur[i].wrapping_add(cur[i - bpp]);
            }
        }
        2 => {
            for i in 0..cur.len() {
                cur[i] = cur[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..cur.len() {
                let left = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                cur[i] = cur[i].wrapping_add(((left + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..cur.len() {
                let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
                let b = prev[i] as i32;
                let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                let p = a + b - c;
                let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                let pr = if pa <= pb && pa <= pc {
                    a
                } else if pb <= pc {
                    b
                } else {
                    c
                };
                cur[i] = cur[i].wrapping_add(pr as u8);
            }
        }
        _ => return Err("bad filter type".into()),
    }
    Ok(())
}

/// Sample `i` of a row at the given bit depth (sub-byte depths are packed MSB first).
#[inline]
fn sample(row: &[u8], i: usize, depth: u8) -> u16 {
    match depth {
        8 => row[i] as u16,
        16 => ((row[2 * i] as u16) << 8) | row[2 * i + 1] as u16,
        d => {
            let per = 8 / d as usize;
            let byte = row[i / per];
            let shift = 8 - d as usize * (i % per + 1);
            ((byte >> shift) & ((1u16 << d) - 1) as u8) as u16
        }
    }
}

fn to8(v: u16, depth: u8) -> u8 {
    match depth {
        16 => (v >> 8) as u8,
        8 => v as u8,
        d => (v as u32 * 255 / ((1u32 << d) - 1)) as u8,
    }
}

fn pixel(row: &[u8], x: usize, h: &Header, ch: usize, pal: &[[u8; 3]], trns: &[u8]) -> [u8; 4] {
    let s = |c: usize| sample(row, x * ch + c, h.depth);
    let key = |i: usize| -> Option<u16> {
        if trns.len() >= 2 * (i + 1) {
            Some(u16::from_be_bytes([trns[2 * i], trns[2 * i + 1]]))
        } else {
            None
        }
    };
    match h.ctype {
        0 => {
            let g = s(0);
            let a = if key(0) == Some(g) { 0 } else { 255 };
            let v = to8(g, h.depth);
            [v, v, v, a]
        }
        2 => {
            let (r, g, b) = (s(0), s(1), s(2));
            let a = if trns.len() >= 6 && key(0) == Some(r) && key(1) == Some(g) && key(2) == Some(b) { 0 } else { 255 };
            [to8(r, h.depth), to8(g, h.depth), to8(b, h.depth), a]
        }
        3 => {
            let i = s(0) as usize;
            let c = pal.get(i).copied().unwrap_or([0, 0, 0]);
            [c[0], c[1], c[2], trns.get(i).copied().unwrap_or(255)]
        }
        4 => {
            let v = to8(s(0), h.depth);
            [v, v, v, to8(s(1), h.depth)]
        }
        _ => [to8(s(0), h.depth), to8(s(1), h.depth), to8(s(2), h.depth), to8(s(3), h.depth)],
    }
}
