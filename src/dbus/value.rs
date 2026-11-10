//! D-Bus values and the wire format (marshalling), little- and big-endian read,
//! little-endian write. Alignment rules follow the D-Bus specification:
//! every value is aligned relative to the start of the message, and because the
//! body always starts 8-aligned, a body can be encoded on its own.

use super::Error;

const MAX_DEPTH: usize = 32;
const MAX_ARRAY: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Path(String),
    Sig(String),
    Fd(u32),
    /// element signature, items (the signature is kept so empty arrays still encode)
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    /// dict entry `{kv}`; only valid as an array element
    Dict(Box<Value>, Box<Value>),
    Variant(Box<Value>),
}

// ---------------------------------------------------------------- building

impl Value {
    pub fn s(x: impl Into<String>) -> Value {
        Value::Str(x.into())
    }
    pub fn path(x: impl Into<String>) -> Value {
        Value::Path(x.into())
    }
    pub fn v(x: Value) -> Value {
        Value::Variant(Box::new(x))
    }
    pub fn strings<I, S>(items: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Value::Array("s".into(), items.into_iter().map(|s| Value::Str(s.into())).collect())
    }
    /// `a{sv}` from (key, value) pairs; each value is wrapped in a variant.
    pub fn dict_sv<I>(entries: I) -> Value
    where
        I: IntoIterator<Item = (String, Value)>,
    {
        Value::Array(
            "{sv}".into(),
            entries
                .into_iter()
                .map(|(k, v)| Value::Dict(Box::new(Value::Str(k)), Box::new(Value::v(v))))
                .collect(),
        )
    }

    /// The D-Bus type signature of this value.
    pub fn signature(&self) -> String {
        let mut s = String::new();
        self.write_sig(&mut s);
        s
    }

    fn write_sig(&self, out: &mut String) {
        match self {
            Value::Byte(_) => out.push('y'),
            Value::Bool(_) => out.push('b'),
            Value::I16(_) => out.push('n'),
            Value::U16(_) => out.push('q'),
            Value::I32(_) => out.push('i'),
            Value::U32(_) => out.push('u'),
            Value::I64(_) => out.push('x'),
            Value::U64(_) => out.push('t'),
            Value::F64(_) => out.push('d'),
            Value::Str(_) => out.push('s'),
            Value::Path(_) => out.push('o'),
            Value::Sig(_) => out.push('g'),
            Value::Fd(_) => out.push('h'),
            Value::Array(e, _) => {
                out.push('a');
                out.push_str(e);
            }
            Value::Struct(items) => {
                out.push('(');
                for i in items {
                    i.write_sig(out);
                }
                out.push(')');
            }
            Value::Dict(k, v) => {
                out.push('{');
                k.write_sig(out);
                v.write_sig(out);
                out.push('}');
            }
            Value::Variant(_) => out.push('v'),
        }
    }
}

/// Signature of a whole argument list.
pub fn signature_of(body: &[Value]) -> String {
    let mut s = String::new();
    for v in body {
        v.write_sig(&mut s);
    }
    s
}

// ---------------------------------------------------------------- reading

impl Value {
    /// Peel any number of variant wrappers.
    pub fn inner(&self) -> &Value {
        let mut v = self;
        while let Value::Variant(b) = v {
            v = b;
        }
        v
    }
    pub fn as_str(&self) -> Option<&str> {
        match self.inner() {
            Value::Str(s) | Value::Path(s) | Value::Sig(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self.inner() {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Any integer type, widened.
    pub fn as_i64(&self) -> Option<i64> {
        Some(match self.inner() {
            Value::Byte(x) => *x as i64,
            Value::I16(x) => *x as i64,
            Value::U16(x) => *x as i64,
            Value::I32(x) => *x as i64,
            Value::U32(x) => *x as i64,
            Value::I64(x) => *x,
            Value::U64(x) => *x as i64,
            _ => return None,
        })
    }
    pub fn as_u32(&self) -> Option<u32> {
        self.as_i64().and_then(|x| u32::try_from(x).ok())
    }
    /// A double, or any integer converted.
    pub fn as_f64(&self) -> Option<f64> {
        match self.inner() {
            Value::F64(x) => Some(*x),
            other => other.as_i64().map(|x| x as f64),
        }
    }
    pub fn as_array(&self) -> Option<&[Value]> {
        match self.inner() {
            Value::Array(_, items) => Some(items),
            _ => None,
        }
    }
    pub fn as_struct(&self) -> Option<&[Value]> {
        match self.inner() {
            Value::Struct(items) => Some(items),
            _ => None,
        }
    }
    /// Strings of an `as` / `ao` array (anything else yields an empty list).
    pub fn as_strings(&self) -> Vec<String> {
        self.as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }
    /// Entries of a dict array (`a{s?}`) as (key, value) pairs.
    pub fn dict_entries(&self) -> Vec<(&str, &Value)> {
        let mut out = Vec::new();
        if let Some(items) = self.as_array() {
            for it in items {
                if let Value::Dict(k, v) = it {
                    if let Some(k) = k.as_str() {
                        out.push((k, &**v));
                    }
                }
            }
        }
        out
    }
    /// Look a key up in an `a{sv}`; the variant wrapper is removed.
    pub fn dict_get(&self, key: &str) -> Option<&Value> {
        self.dict_entries().into_iter().find(|(k, _)| *k == key).map(|(_, v)| v.inner())
    }
}

// ---------------------------------------------------------------- encoding

pub(crate) fn align_of(c: u8) -> usize {
    match c {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

pub struct Encoder {
    pub buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder { buf: Vec::new() }
    }

    pub fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.buf.push(0);
        }
    }

    fn put_u32(&mut self, x: u32) {
        self.align(4);
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    fn put_str(&mut self, s: &str) -> Result<(), Error> {
        if s.len() > u32::MAX as usize - 1 || s.as_bytes().contains(&0) {
            return Err(Error::Protocol("string too long or contains NUL".into()));
        }
        self.put_u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    pub fn write(&mut self, v: &Value) -> Result<(), Error> {
        match v {
            Value::Byte(x) => self.buf.push(*x),
            Value::Bool(x) => self.put_u32(*x as u32),
            Value::I16(x) => {
                self.align(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U16(x) => {
                self.align(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::I32(x) => {
                self.align(4);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U32(x) | Value::Fd(x) => self.put_u32(*x),
            Value::I64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::F64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::Str(s) | Value::Path(s) => self.put_str(s)?,
            Value::Sig(s) => {
                if s.len() > 255 {
                    return Err(Error::Protocol("signature too long".into()));
                }
                self.buf.push(s.len() as u8);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Array(elem, items) => {
                let first = elem.as_bytes().first().copied().unwrap_or(b'y');
                self.put_u32(0);
                let len_at = self.buf.len() - 4;
                self.align(align_of(first));
                let start = self.buf.len();
                for it in items {
                    self.write(it)?;
                }
                let len = self.buf.len() - start;
                if len > MAX_ARRAY {
                    return Err(Error::Protocol("array too large".into()));
                }
                self.buf[len_at..len_at + 4].copy_from_slice(&(len as u32).to_le_bytes());
            }
            Value::Struct(items) => {
                self.align(8);
                for it in items {
                    self.write(it)?;
                }
            }
            Value::Dict(k, val) => {
                self.align(8);
                self.write(k)?;
                self.write(val)?;
            }
            Value::Variant(inner) => {
                let sig = inner.signature();
                if sig.len() > 255 {
                    return Err(Error::Protocol("variant signature too long".into()));
                }
                self.buf.push(sig.len() as u8);
                self.buf.extend_from_slice(sig.as_bytes());
                self.buf.push(0);
                self.write(inner)?;
            }
        }
        Ok(())
    }
}

/// Encode an argument list as a message body.
pub fn encode_body(body: &[Value]) -> Result<Vec<u8>, Error> {
    let mut e = Encoder::new();
    for v in body {
        e.write(v)?;
    }
    Ok(e.buf)
}

// ---------------------------------------------------------------- decoding

/// Index just past the single complete type that starts at `sig[i]`.
pub fn type_end(sig: &[u8], i: usize) -> Option<usize> {
    fn go(sig: &[u8], i: usize, depth: usize) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        match *sig.get(i)? {
            b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g'
            | b'v' | b'h' => Some(i + 1),
            b'a' => go(sig, i + 1, depth + 1),
            b'(' => {
                let mut j = i + 1;
                if *sig.get(j)? == b')' {
                    return None; // empty structs are not allowed
                }
                while *sig.get(j)? != b')' {
                    j = go(sig, j, depth + 1)?;
                }
                Some(j + 1)
            }
            b'{' => {
                let k = *sig.get(i + 1)?;
                if !b"ybnqiuxtdsogh".contains(&k) {
                    return None;
                }
                let j = go(sig, i + 2, depth + 1)?;
                if *sig.get(j)? == b'}' {
                    Some(j + 1)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    go(sig, i, 0)
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pub pos: usize,
    big: bool,
}

fn short() -> Error {
    Error::Protocol("message truncated".into())
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8], big: bool) -> Self {
        Decoder { buf, pos: 0, big }
    }

    fn align(&mut self, n: usize) -> Result<(), Error> {
        self.pos = (self.pos + n - 1) & !(n - 1);
        if self.pos > self.buf.len() {
            return Err(short());
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or_else(short)?;
        if end > self.buf.len() {
            return Err(short());
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn num<const N: usize>(&mut self, align: usize) -> Result<[u8; N], Error> {
        self.align(align)?;
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        if self.big {
            a.reverse(); // so from_le_bytes below works for both
        }
        Ok(a)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.num::<4>(4)?))
    }

    fn string(&mut self) -> Result<String, Error> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        let nul = self.take(1)?;
        if nul[0] != 0 {
            return Err(Error::Protocol("string not NUL terminated".into()));
        }
        Ok(String::from_utf8_lossy(s).into_owned())
    }

    /// Read one value per complete type in `sig`.
    pub fn read_sig(&mut self, sig: &str) -> Result<Vec<Value>, Error> {
        let sig = sig.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < sig.len() {
            let end = type_end(sig, i).ok_or_else(|| Error::Protocol("bad signature".into()))?;
            out.push(self.read_type(&sig[i..end], 0)?);
            i = end;
        }
        Ok(out)
    }

    /// `sig` is exactly one complete type.
    pub fn read_type(&mut self, sig: &[u8], depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Protocol("nesting too deep".into()));
        }
        Ok(match sig[0] {
            b'y' => Value::Byte(self.take(1)?[0]),
            b'b' => match self.u32()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(Error::Protocol("bad boolean".into())),
            },
            b'n' => Value::I16(i16::from_le_bytes(self.num::<2>(2)?)),
            b'q' => Value::U16(u16::from_le_bytes(self.num::<2>(2)?)),
            b'i' => Value::I32(i32::from_le_bytes(self.num::<4>(4)?)),
            b'u' => Value::U32(self.u32()?),
            b'h' => Value::Fd(self.u32()?),
            b'x' => Value::I64(i64::from_le_bytes(self.num::<8>(8)?)),
            b't' => Value::U64(u64::from_le_bytes(self.num::<8>(8)?)),
            b'd' => Value::F64(f64::from_le_bytes(self.num::<8>(8)?)),
            b's' => Value::Str(self.string()?),
            b'o' => Value::Path(self.string()?),
            b'g' => {
                let n = self.take(1)?[0] as usize;
                let s = self.take(n)?;
                if self.take(1)?[0] != 0 {
                    return Err(Error::Protocol("signature not NUL terminated".into()));
                }
                Value::Sig(String::from_utf8_lossy(s).into_owned())
            }
            b'v' => {
                let n = self.take(1)?[0] as usize;
                let s = self.take(n)?.to_vec();
                if self.take(1)?[0] != 0 {
                    return Err(Error::Protocol("signature not NUL terminated".into()));
                }
                match type_end(&s, 0) {
                    Some(e) if e == s.len() => {}
                    _ => return Err(Error::Protocol("bad variant signature".into())),
                }
                Value::Variant(Box::new(self.read_type(&s, depth + 1)?))
            }
            b'a' => {
                let elem = &sig[1..];
                let len = self.u32()? as usize;
                if len > MAX_ARRAY {
                    return Err(Error::Protocol("array too large".into()));
                }
                self.align(align_of(elem[0]))?;
                let end = self.pos.checked_add(len).ok_or_else(short)?;
                if end > self.buf.len() {
                    return Err(short());
                }
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read_type(elem, depth + 1)?);
                }
                if self.pos != end {
                    return Err(Error::Protocol("array length mismatch".into()));
                }
                Value::Array(String::from_utf8_lossy(elem).into_owned(), items)
            }
            b'(' => {
                self.align(8)?;
                let inner = &sig[1..sig.len() - 1];
                let mut items = Vec::new();
                let mut i = 0;
                while i < inner.len() {
                    let e = type_end(inner, i).ok_or_else(|| Error::Protocol("bad struct".into()))?;
                    items.push(self.read_type(&inner[i..e], depth + 1)?);
                    i = e;
                }
                Value::Struct(items)
            }
            b'{' => {
                self.align(8)?;
                let inner = &sig[1..sig.len() - 1];
                let ke = type_end(inner, 0).ok_or_else(|| Error::Protocol("bad dict".into()))?;
                let k = self.read_type(&inner[..ke], depth + 1)?;
                let v = self.read_type(&inner[ke..], depth + 1)?;
                Value::Dict(Box::new(k), Box::new(v))
            }
            _ => return Err(Error::Protocol("unknown type code".into())),
        })
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) {
        let sig = v.signature();
        let bytes = encode_body(std::slice::from_ref(&v)).unwrap();
        let mut d = Decoder::new(&bytes, false);
        let back = d.read_sig(&sig).unwrap();
        assert_eq!(back, vec![v], "sig {sig}");
        assert_eq!(d.pos, bytes.len(), "all bytes consumed for {sig}");
    }

    #[test]
    fn basic_types_roundtrip() {
        roundtrip(Value::Byte(200));
        roundtrip(Value::Bool(true));
        roundtrip(Value::I16(-1234));
        roundtrip(Value::U16(65000));
        roundtrip(Value::I32(-5));
        roundtrip(Value::U32(0xdead_beef));
        roundtrip(Value::I64(-1 << 40));
        roundtrip(Value::U64(u64::MAX));
        roundtrip(Value::F64(0.1 + 0.2));
        roundtrip(Value::s("héllo 日本語 🎵"));
        roundtrip(Value::path("/org/mpris/MediaPlayer2"));
        roundtrip(Value::Sig("a{sv}".into()));
    }

    #[test]
    fn known_string_encoding() {
        // "hi" => len 2, 'h' 'i', NUL
        let b = encode_body(&[Value::s("hi")]).unwrap();
        assert_eq!(b, vec![2, 0, 0, 0, b'h', b'i', 0]);
        // string after a byte is padded to 4
        let b = encode_body(&[Value::Byte(1), Value::s("a")]).unwrap();
        assert_eq!(b, vec![1, 0, 0, 0, 1, 0, 0, 0, b'a', 0]);
    }

    #[test]
    fn array_of_i64_pads_to_element_alignment_even_when_empty() {
        let b = encode_body(&[Value::Array("x".into(), vec![])]).unwrap();
        // length word (0) at 0..4, then padding to 8 (nothing to write after, but the
        // spec still pads), so total is 8 bytes
        assert_eq!(b, vec![0, 0, 0, 0, 0, 0, 0, 0]);
        let b = encode_body(&[Value::Array("x".into(), vec![Value::I64(1)])]).unwrap();
        assert_eq!(b.len(), 16);
        assert_eq!(&b[0..4], &8u32.to_le_bytes()); // length excludes the pad
    }

    #[test]
    fn nested_containers_roundtrip() {
        roundtrip(Value::Struct(vec![Value::Byte(1), Value::I64(2), Value::s("x")]));
        roundtrip(Value::dict_sv(vec![
            ("title".to_string(), Value::s("曲名")),
            ("length".to_string(), Value::I64(215_000_000)),
            ("artist".to_string(), Value::strings(["a", "b"])),
            ("nested".to_string(), Value::v(Value::U32(7))),
            ("pos".to_string(), Value::Struct(vec![Value::F64(1.5), Value::Bool(false)])),
        ]));
        roundtrip(Value::Array("(yv)".into(), vec![]));
        roundtrip(Value::Array(
            "a{sv}".into(),
            vec![Value::dict_sv(vec![("k".to_string(), Value::I32(1))])],
        ));
        roundtrip(Value::v(Value::v(Value::s("deep"))));
    }

    #[test]
    fn multiple_args_in_one_body() {
        let body = vec![
            Value::s("app"),
            Value::U32(0),
            Value::s(""),
            Value::strings(["default", "Open"]),
            Value::dict_sv(vec![("urgency".to_string(), Value::Byte(1))]),
            Value::I32(-1),
        ];
        let sig = signature_of(&body);
        assert_eq!(sig, "susasa{sv}i");
        let bytes = encode_body(&body).unwrap();
        let back = Decoder::new(&bytes, false).read_sig(&sig).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn big_endian_is_read_correctly() {
        // u32 0x01020304 big endian, then string "ab"
        let mut b = vec![1, 2, 3, 4, 0, 0, 0, 2, b'a', b'b', 0];
        let mut d = Decoder::new(&b, true);
        let v = d.read_sig("us").unwrap();
        assert_eq!(v, vec![Value::U32(0x0102_0304), Value::s("ab")]);
        b.truncate(5);
        assert!(Decoder::new(&b, true).read_sig("us").is_err());
    }

    #[test]
    fn rejects_malformed_input() {
        // string length points past the end
        assert!(Decoder::new(&[255, 0, 0, 0, b'x', 0], false).read_sig("s").is_err());
        // missing NUL
        assert!(Decoder::new(&[1, 0, 0, 0, b'x', b'y'], false).read_sig("s").is_err());
        // bad boolean
        assert!(Decoder::new(&[2, 0, 0, 0], false).read_sig("b").is_err());
        // array longer than the buffer
        assert!(Decoder::new(&[100, 0, 0, 0, 1, 2], false).read_sig("ay").is_err());
        // garbage signatures
        for bad in ["(", "()", "a", "{sv", "{vs}", "z", "a{}", "(i"] {
            assert!(Decoder::new(&[0; 64], false).read_sig(bad).is_err(), "{bad}");
        }
        // absurd nesting
        let deep = format!("{}y{}", "(".repeat(100), ")".repeat(100));
        assert!(Decoder::new(&[0; 512], false).read_sig(&deep).is_err());
    }

    #[test]
    fn type_end_finds_complete_types() {
        assert_eq!(type_end(b"a{sv}i", 0), Some(5));
        assert_eq!(type_end(b"(ia(yv))s", 0), Some(8));
        assert_eq!(type_end(b"aai", 0), Some(3));
        assert_eq!(type_end(b"{sv}", 0), Some(4));
        assert_eq!(type_end(b"", 0), None);
    }

    #[test]
    fn accessors() {
        let d = Value::dict_sv(vec![
            ("Volume".to_string(), Value::F64(0.5)),
            ("Status".to_string(), Value::s("Playing")),
            ("Len".to_string(), Value::I64(12)),
            ("Artists".to_string(), Value::strings(["x", "y"])),
        ]);
        assert_eq!(d.dict_get("Volume").and_then(Value::as_f64), Some(0.5));
        assert_eq!(d.dict_get("Status").and_then(Value::as_str), Some("Playing"));
        assert_eq!(d.dict_get("Len").and_then(Value::as_i64), Some(12));
        assert_eq!(d.dict_get("Len").and_then(Value::as_f64), Some(12.0));
        assert_eq!(d.dict_get("Artists").unwrap().as_strings(), vec!["x", "y"]);
        assert!(d.dict_get("Nope").is_none());
        assert_eq!(Value::v(Value::v(Value::U32(3))).as_u32(), Some(3));
        assert_eq!(Value::I32(-1).as_u32(), None);
    }
}
