//! zlib / DEFLATE decompression (RFC 1950, 1951).

const MAX_BITS: usize = 15;
const FAST_BITS: u32 = 10;

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0, acc: 0, n: 0 }
    }
    #[inline]
    fn refill(&mut self) {
        while self.n <= 56 {
            if self.pos < self.d.len() {
                self.acc |= (self.d[self.pos] as u64) << self.n;
                self.pos += 1;
            } else {
                // past the end: zero bits, detected later through `overrun`
                self.pos += 1;
            }
            self.n += 8;
        }
    }
    /// Bytes consumed beyond the input (zero padding used).
    fn overrun(&self) -> bool {
        // pos counts padding too; real bits left = n, padded bytes = pos - len
        self.pos > self.d.len() && ((self.pos - self.d.len()) * 8) as u32 > self.n
    }
    #[inline]
    fn get(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        if self.n < k {
            self.refill();
        }
        let v = (self.acc & ((1u64 << k) - 1)) as u32;
        self.acc >>= k;
        self.n -= k;
        v
    }
    fn align_byte(&mut self) {
        let drop = self.n % 8;
        self.acc >>= drop;
        self.n -= drop;
    }
}

struct Huff {
    /// fast[(code reversed, FAST_BITS)] = symbol << 4 | length, 0 if longer
    fast: Vec<u16>,
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Result<Huff, String> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        count[0] = 0;
        // over-subscribed or incomplete sets are rejected (a single code of length 1 is allowed)
        let mut left = 1i32;
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= count[len] as i32;
            if left < 0 {
                return Err("over-subscribed Huffman code".into());
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        // fast table
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut code = 0u32;
        let mut idx = 0usize;
        for len in 1..=MAX_BITS {
            for _ in 0..count[len] {
                if len as u32 <= FAST_BITS {
                    // bit-reverse the code (deflate sends Huffman codes MSB first)
                    let mut rev = 0u32;
                    for b in 0..len {
                        if code & (1 << b) != 0 {
                            rev |= 1 << (len - 1 - b);
                        }
                    }
                    let entry = (symbol[idx] << 4) | len as u16;
                    let mut i = rev as usize;
                    while i < fast.len() {
                        fast[i] = entry;
                        i += 1 << len;
                    }
                }
                code += 1;
                idx += 1;
            }
            code <<= 1;
        }
        Ok(Huff { fast, count, symbol })
    }

    #[inline]
    fn decode(&self, b: &mut Bits) -> Result<u16, String> {
        if b.n < 16 {
            b.refill();
        }
        let e = self.fast[(b.acc & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            let len = (e & 15) as u32;
            b.acc >>= len;
            b.n -= len;
            return Ok(e >> 4);
        }
        // slow canonical decode, bit by bit
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        let mut acc = b.acc;
        for len in 1..=MAX_BITS {
            code |= (acc & 1) as i32;
            acc >>= 1;
            let c = self.count[len] as i32;
            if code - c < first {
                b.acc >>= len;
                b.n -= len as u32;
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += c;
            first += c;
            first <<= 1;
            code <<= 1;
        }
        Err("invalid Huffman code".into())
    }
}

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Raw DEFLATE data. `max_out` bounds the output size (decompression bombs).
pub fn inflate(data: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(3).min(max_out).min(1 << 26));
    let mut b = Bits::new(data);
    loop {
        let last = b.get(1);
        let kind = b.get(2);
        match kind {
            0 => {
                b.align_byte();
                let len = b.get(16) as usize;
                let nlen = b.get(16) as usize;
                if len != (!nlen & 0xffff) {
                    return Err("stored block length mismatch".into());
                }
                if out.len() + len > max_out {
                    return Err("output too large".into());
                }
                for _ in 0..len {
                    out.push(b.get(8) as u8);
                }
            }
            1 | 2 => {
                let (lit, dist) = if kind == 1 {
                    let mut l = [0u8; 288];
                    for (i, v) in l.iter_mut().enumerate() {
                        *v = match i {
                            0..=143 => 8,
                            144..=255 => 9,
                            256..=279 => 7,
                            _ => 8,
                        };
                    }
                    (Huff::new(&l)?, Huff::new(&[5u8; 30])?)
                } else {
                    dynamic_tables(&mut b)?
                };
                loop {
                    let sym = lit.decode(&mut b)? as usize;
                    if sym < 256 {
                        if out.len() >= max_out {
                            return Err("output too large".into());
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let s = sym - 257;
                        if s >= 29 {
                            return Err("bad length symbol".into());
                        }
                        let len = LEN_BASE[s] as usize + b.get(LEN_EXTRA[s] as u32) as usize;
                        let ds = dist.decode(&mut b)? as usize;
                        if ds >= 30 {
                            return Err("bad distance symbol".into());
                        }
                        let d = DIST_BASE[ds] as usize + b.get(DIST_EXTRA[ds] as u32) as usize;
                        if d > out.len() {
                            return Err("distance too far back".into());
                        }
                        if out.len() + len > max_out {
                            return Err("output too large".into());
                        }
                        let start = out.len() - d;
                        if d >= len {
                            out.extend_from_within(start..start + len);
                        } else {
                            for i in 0..len {
                                let v = out[start + i];
                                out.push(v);
                            }
                        }
                    }
                    if b.overrun() {
                        return Err("unexpected end of data".into());
                    }
                }
            }
            _ => return Err("bad block type".into()),
        }
        if b.overrun() {
            return Err("unexpected end of data".into());
        }
        if last == 1 {
            break;
        }
    }
    Ok(out)
}

fn dynamic_tables(b: &mut Bits) -> Result<(Huff, Huff), String> {
    let hlit = b.get(5) as usize + 257;
    let hdist = b.get(5) as usize + 1;
    let hclen = b.get(4) as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err("bad table sizes".into());
    }
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let mut cl = [0u8; 19];
    for &o in ORDER.iter().take(hclen) {
        cl[o] = b.get(3) as u8;
    }
    let clh = Huff::new(&cl)?;
    let mut lens = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lens.len() {
        let sym = clh.decode(b)?;
        match sym {
            0..=15 => {
                lens[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("repeat with no previous length".into());
                }
                let prev = lens[i - 1];
                let n = 3 + b.get(2) as usize;
                if i + n > lens.len() {
                    return Err("repeat overflows".into());
                }
                lens[i..i + n].fill(prev);
                i += n;
            }
            17 | 18 => {
                let n = if sym == 17 { 3 + b.get(3) as usize } else { 11 + b.get(7) as usize };
                if i + n > lens.len() {
                    return Err("repeat overflows".into());
                }
                i += n; // already zero
            }
            _ => return Err("bad code length symbol".into()),
        }
        if b.overrun() {
            return Err("unexpected end of data".into());
        }
    }
    if lens[256] == 0 {
        return Err("no end-of-block code".into());
    }
    Ok((Huff::new(&lens[..hlit])?, Huff::new(&lens[hlit..])?))
}

/// zlib stream (2 byte header, deflate data, Adler-32 trailer, which is not verified).
pub fn zlib_decompress(data: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    if data.len() < 2 {
        return Err("zlib stream too short".into());
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 || ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err("bad zlib header".into());
    }
    if flg & 0x20 != 0 {
        return Err("preset dictionaries are not supported".into());
    }
    inflate(&data[2..], max_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_and_fixed_blocks() {
        // stored: "abc"
        assert_eq!(inflate(&[1, 3, 0, 0xfc, 0xff, b'a', b'b', b'c'], 100).unwrap(), b"abc");
        // fixed Huffman, generated by zlib for b"hello hello hello hello"
        let z = [0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08, 0xb1];
        assert_eq!(zlib_decompress(&z, 100).unwrap(), b"hello hello hello hello");
    }

    #[test]
    fn limits_and_errors() {
        assert!(inflate(&[1, 3, 0, 0xfc, 0xff, b'a', b'b', b'c'], 2).is_err(), "bomb limit");
        assert!(inflate(&[1, 3, 0, 0, 0, b'a'], 100).is_err(), "length check");
        assert!(inflate(&[], 100).is_err());
        assert!(inflate(&[0xff, 0xff, 0xff], 100).is_err());
        assert!(zlib_decompress(&[0x78], 10).is_err());
        assert!(zlib_decompress(&[0x00, 0x00, 1], 10).is_err());
        // truncated stream
        let z = [0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01];
        for cut in 2..z.len() {
            let _ = zlib_decompress(&z[..cut], 100); // must not panic
        }
    }

    #[test]
    fn random_garbage_never_panics_or_hangs() {
        let mut s: u32 = 7;
        for _ in 0..2000 {
            let len = (s % 64) as usize;
            let mut v = Vec::new();
            for _ in 0..len {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                v.push((s >> 24) as u8);
            }
            let _ = inflate(&v, 1 << 16);
        }
    }
}
