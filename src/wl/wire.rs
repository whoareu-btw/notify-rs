//! Wayland wire format: 32-bit words in native byte order. A message is
//! `object id`, then `size << 16 | opcode`, then the arguments padded to
//! 4 bytes. File descriptors travel out of band (SCM_RIGHTS).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub obj: u32,
    pub op: u16,
    pub data: Vec<u8>,
}

/// Request arguments under construction.
#[derive(Default, Clone, Debug)]
pub struct Args(Vec<u8>);

impl Args {
    pub fn new() -> Args {
        Args(Vec::new())
    }
    /// uint, object id, new_id
    pub fn u(mut self, v: u32) -> Args {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn i(mut self, v: i32) -> Args {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    /// 24.8 signed fixed point
    pub fn fixed(self, v: f64) -> Args {
        self.i(to_fixed(v))
    }
    /// string: length including the NUL, bytes, NUL, padding
    pub fn s(mut self, v: &str) -> Args {
        let bytes: Vec<u8> = v.bytes().filter(|&b| b != 0).collect();
        self.0.extend_from_slice(&((bytes.len() + 1) as u32).to_ne_bytes());
        self.0.extend_from_slice(&bytes);
        self.0.push(0);
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
        self
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn to_fixed(v: f64) -> i32 {
    (v * 256.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

pub fn from_fixed(v: i32) -> f64 {
    v as f64 / 256.0
}

pub fn encode(obj: u32, op: u16, args: &Args) -> Vec<u8> {
    let size = 8 + args.0.len();
    assert!(size <= 0xffff, "Wayland message too large");
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(&obj.to_ne_bytes());
    v.extend_from_slice(&(((size as u32) << 16) | op as u32).to_ne_bytes());
    v.extend_from_slice(&args.0);
    v
}

/// Event arguments, read front to back. Every method returns None instead of
/// panicking when the message is shorter than its type says.
pub struct Reader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(d: &'a [u8]) -> Reader<'a> {
        Reader { d, pos: 0 }
    }
    pub fn u(&mut self) -> Option<u32> {
        let b = self.d.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i(&mut self) -> Option<i32> {
        self.u().map(|v| v as i32)
    }
    pub fn fixed(&mut self) -> Option<f64> {
        self.i().map(from_fixed)
    }
    /// A string; the null string reads as "".
    pub fn string(&mut self) -> Option<String> {
        let len = self.u()? as usize;
        if len == 0 {
            return Some(String::new());
        }
        let b = self.d.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += (len + 3) & !3;
        let text = &b[..len - 1];
        Some(String::from_utf8_lossy(text).into_owned())
    }
    pub fn array(&mut self) -> Option<&'a [u8]> {
        let len = self.u()? as usize;
        let b = self.d.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += (len + 3) & !3;
        Some(b)
    }
    pub fn done(&self) -> bool {
        self.pos >= self.d.len()
    }
}

/// Take one complete message from the front of `buf`: the message and how many
/// bytes it used. `Ok(None)` means more data is needed.
pub fn next_msg(buf: &[u8]) -> Result<Option<(Msg, usize)>, String> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let obj = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let w = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let (size, op) = ((w >> 16) as usize, (w & 0xffff) as u16);
    if size < 8 || size % 4 != 0 {
        return Err(format!("malformed message (size {size})"));
    }
    if buf.len() < size {
        return Ok(None);
    }
    Ok(Some((Msg { obj, op, data: buf[8..size].to_vec() }, size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let m = encode(7, 3, &Args::new().u(1).i(-2));
        assert_eq!(m.len(), 16);
        assert_eq!(&m[0..4], &7u32.to_ne_bytes());
        assert_eq!(&m[4..8], &((16u32 << 16) | 3).to_ne_bytes());
        assert_eq!(&m[8..12], &1u32.to_ne_bytes());
        assert_eq!(&m[12..16], &(-2i32).to_ne_bytes());
    }

    #[test]
    fn strings_are_nul_terminated_and_padded() {
        // "wl_shm" is 6 bytes + NUL = 7, padded to 8
        let a = Args::new().s("wl_shm");
        assert_eq!(a.len(), 4 + 8);
        let m = encode(2, 0, &a);
        let (msg, used) = next_msg(&m).unwrap().unwrap();
        assert_eq!(used, m.len());
        let mut r = Reader::new(&msg.data);
        assert_eq!(r.string().unwrap(), "wl_shm");
        assert!(r.done());
        // exactly 3 bytes + NUL = 4: no padding
        assert_eq!(Args::new().s("abc").len(), 8);
        // empty string still has the NUL
        assert_eq!(Args::new().s("").len(), 8);
        let empty = Args::new().s("");
        let mut r = Reader::new(&empty.0);
        assert_eq!(r.string().unwrap(), "");
    }

    #[test]
    fn fixed_point_round_trips() {
        for v in [0.0, 1.0, -1.0, 12.5, -300.25, 1919.99609375] {
            assert_eq!(from_fixed(to_fixed(v)), v);
        }
        assert_eq!(to_fixed(1.0), 256);
        assert_eq!(to_fixed(-0.5), -128);
        assert!((from_fixed(to_fixed(0.3)) - 0.3).abs() < 1.0 / 256.0);
    }

    #[test]
    fn framing_handles_partial_and_batched_data() {
        let a = encode(1, 0, &Args::new().u(9));
        let b = encode(2, 1, &Args::new().s("hi").u(4));
        let mut all = a.clone();
        all.extend_from_slice(&b);
        // every split point
        for cut in 0..all.len() {
            let mut buf = all[..cut].to_vec();
            let mut got = Vec::new();
            loop {
                match next_msg(&buf).unwrap() {
                    Some((m, n)) => {
                        got.push(m);
                        buf.drain(..n);
                    }
                    None => break,
                }
            }
            assert!(got.len() <= 2);
            if cut >= a.len() {
                assert_eq!(got[0], Msg { obj: 1, op: 0, data: 9u32.to_ne_bytes().to_vec() });
            }
        }
        let (m1, n1) = next_msg(&all).unwrap().unwrap();
        let (m2, n2) = next_msg(&all[n1..]).unwrap().unwrap();
        assert_eq!((m1.obj, m2.obj, n1 + n2), (1, 2, all.len()));
    }

    #[test]
    fn malformed_input_is_an_error_or_none() {
        assert!(next_msg(&[1, 0, 0, 0, 4, 0, 0, 0]).is_err()); // size 4 < header
        assert!(next_msg(&[1, 0, 0, 0, 2, 0, 9, 0]).is_err()); // size 9 not a multiple of 4
        assert!(next_msg(&[1, 0, 0]).unwrap().is_none());
        let mut r = Reader::new(&[5, 0, 0, 0]);
        assert!(r.string().is_none(), "length with no bytes");
        assert!(Reader::new(&[]).u().is_none());
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3, 4]).string().is_none(), "absurd length");
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff]).array().is_none());
    }
}
