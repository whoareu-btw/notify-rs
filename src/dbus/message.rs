//! D-Bus messages: header fields, encode, decode.

use super::value::{encode_body, signature_of, Decoder, Encoder, Value};
use super::Error;

pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;
pub const NO_REPLY_EXPECTED: u8 = 0x1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
    Other(u8),
}

impl MsgType {
    fn code(self) -> u8 {
        match self {
            MsgType::MethodCall => 1,
            MsgType::MethodReturn => 2,
            MsgType::Error => 3,
            MsgType::Signal => 4,
            MsgType::Other(c) => c,
        }
    }
    fn from_code(c: u8) -> MsgType {
        match c {
            1 => MsgType::MethodCall,
            2 => MsgType::MethodReturn,
            3 => MsgType::Error,
            4 => MsgType::Signal,
            c => MsgType::Other(c),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub mtype: MsgType,
    pub flags: u8,
    /// Assigned by the connection when sent; 0 until then.
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
}

impl Message {
    fn blank(mtype: MsgType) -> Message {
        Message {
            mtype,
            flags: 0,
            serial: 0,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: None,
            body: Vec::new(),
        }
    }

    pub fn call(dest: &str, path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        let mut m = Message::blank(MsgType::MethodCall);
        m.destination = Some(dest.to_owned());
        m.path = Some(path.to_owned());
        m.interface = Some(iface.to_owned());
        m.member = Some(member.to_owned());
        m.body = body;
        m
    }

    pub fn signal(path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        let mut m = Message::blank(MsgType::Signal);
        m.path = Some(path.to_owned());
        m.interface = Some(iface.to_owned());
        m.member = Some(member.to_owned());
        m.body = body;
        m
    }

    /// A successful reply to `call`.
    pub fn reply_to(call: &Message, body: Vec<Value>) -> Message {
        let mut m = Message::blank(MsgType::MethodReturn);
        m.reply_serial = Some(call.serial);
        m.destination = call.sender.clone();
        m.flags = NO_REPLY_EXPECTED;
        m.body = body;
        m
    }

    /// An error reply to `call`.
    pub fn error_to(call: &Message, name: &str, text: &str) -> Message {
        let mut m = Message::blank(MsgType::Error);
        m.reply_serial = Some(call.serial);
        m.destination = call.sender.clone();
        m.error_name = Some(name.to_owned());
        m.flags = NO_REPLY_EXPECTED;
        m.body = vec![Value::s(text)];
        m
    }

    pub fn signature(&self) -> String {
        signature_of(&self.body)
    }

    pub fn expects_reply(&self) -> bool {
        self.mtype == MsgType::MethodCall && self.flags & NO_REPLY_EXPECTED == 0
    }

    /// True if this is a method call to `iface.member`.
    pub fn is_call(&self, iface: &str, member: &str) -> bool {
        self.mtype == MsgType::MethodCall
            && self.interface.as_deref() == Some(iface)
            && self.member.as_deref() == Some(member)
    }

    /// True if this is the signal `iface.member`.
    pub fn is_signal(&self, iface: &str, member: &str) -> bool {
        self.mtype == MsgType::Signal
            && self.interface.as_deref() == Some(iface)
            && self.member.as_deref() == Some(member)
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let body = encode_body(&self.body)?;
        let sig = self.signature();

        let mut fields: Vec<Value> = Vec::new();
        let mut field = |code: u8, v: Value| {
            fields.push(Value::Struct(vec![Value::Byte(code), Value::v(v)]));
        };
        if let Some(p) = &self.path {
            field(1, Value::path(p.clone()));
        }
        if let Some(i) = &self.interface {
            field(2, Value::s(i.clone()));
        }
        if let Some(m) = &self.member {
            field(3, Value::s(m.clone()));
        }
        if let Some(e) = &self.error_name {
            field(4, Value::s(e.clone()));
        }
        if let Some(r) = self.reply_serial {
            field(5, Value::U32(r));
        }
        if let Some(d) = &self.destination {
            field(6, Value::s(d.clone()));
        }
        if let Some(s) = &self.sender {
            field(7, Value::s(s.clone()));
        }
        if !sig.is_empty() {
            field(8, Value::Sig(sig));
        }

        let mut e = Encoder::new();
        e.buf.extend_from_slice(&[b'l', self.mtype.code(), self.flags, 1]);
        e.buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        e.buf.extend_from_slice(&self.serial.to_le_bytes());
        e.write(&Value::Array("(yv)".into(), fields))?;
        e.align(8);
        e.buf.extend_from_slice(&body);
        if e.buf.len() > MAX_MESSAGE {
            return Err(Error::Protocol("message too large".into()));
        }
        Ok(e.buf)
    }

    /// Try to take one message off the front of `buf`. `Ok(None)` means more
    /// bytes are needed; on success the second value is how many bytes were used.
    pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, Error> {
        if buf.len() < 16 {
            return Ok(None);
        }
        let big = match buf[0] {
            b'l' => false,
            b'B' => true,
            _ => return Err(Error::Protocol("bad endianness byte".into())),
        };
        if buf[3] != 1 {
            return Err(Error::Protocol("unsupported protocol version".into()));
        }
        let rd = |o: usize| -> usize {
            let b = [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]];
            (if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }) as usize
        };
        let body_len = rd(4);
        let serial = rd(8) as u32;
        let fields_len = rd(12);
        if body_len > MAX_MESSAGE || fields_len > 1 << 20 {
            return Err(Error::Protocol("message too large".into()));
        }
        let hdr_end = 16 + fields_len;
        let body_start = (hdr_end + 7) & !7;
        let total = body_start + body_len;
        if buf.len() < total {
            return Ok(None);
        }

        let mut d = Decoder::new(&buf[..hdr_end], big);
        d.pos = 12;
        let fields = d.read_type(b"a(yv)", 0)?;

        let mut m = Message::blank(MsgType::from_code(buf[1]));
        m.flags = buf[2];
        m.serial = serial;
        let mut sig = String::new();
        for f in fields.as_array().unwrap_or(&[]) {
            let Some([code, val]) = f.as_struct() else { continue };
            let Value::Byte(code) = code else { continue };
            let val = val.inner();
            match *code {
                1 => m.path = val.as_str().map(str::to_owned),
                2 => m.interface = val.as_str().map(str::to_owned),
                3 => m.member = val.as_str().map(str::to_owned),
                4 => m.error_name = val.as_str().map(str::to_owned),
                5 => m.reply_serial = val.as_u32(),
                6 => m.destination = val.as_str().map(str::to_owned),
                7 => m.sender = val.as_str().map(str::to_owned),
                8 => sig = val.as_str().unwrap_or("").to_owned(),
                _ => {} // unknown fields must be ignored
            }
        }
        let mut bd = Decoder::new(&buf[body_start..total], big);
        m.body = bd.read_sig(&sig)?;
        Ok(Some((m, total)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_roundtrip() {
        let mut m = Message::call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            vec![Value::s("dev.notifyrs.Daemon"), Value::U32(4)],
        );
        m.serial = 7;
        let b = m.encode().unwrap();
        let body_len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        assert_eq!((b.len() - body_len) % 8, 0, "header ends 8-aligned, body follows");
        let (back, used) = Message::decode(&b).unwrap().unwrap();
        assert_eq!(used, b.len());
        assert_eq!(back, m);
    }

    #[test]
    fn partial_input_asks_for_more() {
        let mut m = Message::signal("/a", "a.B", "C", vec![Value::s("x")]);
        m.serial = 1;
        let b = m.encode().unwrap();
        for cut in [0, 5, 15, 16, b.len() - 1] {
            assert!(Message::decode(&b[..cut]).unwrap().is_none(), "cut {cut}");
        }
        // two messages back to back
        let mut two = b.clone();
        two.extend_from_slice(&b);
        let (_, used) = Message::decode(&two).unwrap().unwrap();
        assert_eq!(used, b.len());
    }

    #[test]
    fn error_and_reply_target_the_caller() {
        let mut call = Message::call("x.y", "/p", "i.f", "M", vec![]);
        call.serial = 42;
        call.sender = Some(":1.9".into());
        let r = Message::reply_to(&call, vec![Value::U32(1)]);
        assert_eq!(r.reply_serial, Some(42));
        assert_eq!(r.destination.as_deref(), Some(":1.9"));
        let e = Message::error_to(&call, "x.Err", "no");
        let b = {
            let mut e = e.clone();
            e.serial = 3;
            e.encode().unwrap()
        };
        let (back, _) = Message::decode(&b).unwrap().unwrap();
        assert_eq!(back.error_name.as_deref(), Some("x.Err"));
        assert_eq!(back.body[0].as_str(), Some("no"));
    }

    #[test]
    fn unknown_header_fields_and_types_are_tolerated() {
        // hand-built message: type 9 (unknown), one unknown field code 99
        let mut e = Encoder::new();
        e.buf.extend_from_slice(&[b'l', 9, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0]);
        let fields = Value::Array(
            "(yv)".into(),
            vec![Value::Struct(vec![Value::Byte(99), Value::v(Value::U32(5))])],
        );
        e.write(&fields).unwrap();
        e.align(8);
        let (m, _) = Message::decode(&e.buf).unwrap().unwrap();
        assert_eq!(m.mtype, MsgType::Other(9));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(Message::decode(&[b'x'; 32]).is_err());
        let mut b = vec![b'l', 1, 0, 1, 0xff, 0xff, 0xff, 0x7f, 1, 0, 0, 0, 0, 0, 0, 0];
        assert!(Message::decode(&b).is_err()); // body too big
        b[4..8].copy_from_slice(&0u32.to_le_bytes());
        b[12..16].copy_from_slice(&0xffffu32.to_le_bytes());
        assert!(Message::decode(&b).unwrap().is_none()); // waits for header bytes
    }
}
