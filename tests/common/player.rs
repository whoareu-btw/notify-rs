//! A fake MPRIS player on the session bus, shared by the integration tests.
#![allow(dead_code)]

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use notify_rs::dbus::{Conn, Message, MsgType, Value};
use notify_rs::mpris::{IFACE_PLAYER, OBJ};

#[derive(Clone)]
pub struct State {
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: String,
    pub length: i64,
    pub volume: f64,
    pub position: i64,
    pub position_in_getall: bool,
    pub metadata_empty: bool,
    pub calls: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            status: "Paused".into(),
            title: "Song".into(),
            artist: "Band".into(),
            album: "Album".into(),
            art: "file:///tmp/x.png".into(),
            length: 200_000_000,
            volume: 0.5,
            position: 12_000_000,
            position_in_getall: true,
            metadata_empty: false,
            calls: vec![],
        }
    }
}

pub struct Mock {
    pub conn: Conn,
    pub state: Arc<Mutex<State>>,
}

impl Mock {
    pub fn metadata(s: &State) -> Value {
        if s.metadata_empty {
            return Value::dict_sv(vec![]);
        }
        Value::dict_sv(vec![
            ("mpris:trackid".into(), Value::path("/org/mock/track/1")),
            ("mpris:length".into(), Value::I64(s.length)),
            ("mpris:artUrl".into(), Value::s(s.art.clone())),
            ("xesam:title".into(), Value::s(s.title.clone())),
            ("xesam:artist".into(), Value::strings([s.artist.clone()])),
            ("xesam:album".into(), Value::s(s.album.clone())),
        ])
    }

    pub fn player_props(s: &State) -> Vec<(String, Value)> {
        let mut v = vec![
            ("PlaybackStatus".to_string(), Value::s(s.status.clone())),
            ("Metadata".to_string(), Mock::metadata(s)),
            ("Volume".to_string(), Value::F64(s.volume)),
            ("CanSeek".to_string(), Value::Bool(true)),
        ];
        if s.position_in_getall {
            v.push(("Position".to_string(), Value::I64(s.position)));
        }
        v
    }

    pub fn start(address: &str, name: &str, init: State) -> Mock {
        let (conn, rx) = Conn::connect(address).unwrap();
        assert_eq!(conn.request_name(name, 4).unwrap(), 1);
        let state = Arc::new(Mutex::new(init));
        let st = state.clone();
        let c = conn.clone();
        std::thread::spawn(move || Mock::serve(c, rx, st));
        Mock { conn, state }
    }

    pub fn serve(conn: Conn, rx: Receiver<Message>, st: Arc<Mutex<State>>) {
        while let Ok(m) = rx.recv() {
            if m.mtype != MsgType::MethodCall {
                continue;
            }
            let iface = m.interface.clone().unwrap_or_default();
            let member = m.member.clone().unwrap_or_default();
            let mut s = st.lock().unwrap();
            match (iface.as_str(), member.as_str()) {
                ("org.freedesktop.DBus.Properties", "GetAll") => {
                    let which = m.body[0].as_str().unwrap_or("").to_owned();
                    let props = match which.as_str() {
                        IFACE_PLAYER => Mock::player_props(&s),
                        "org.mpris.MediaPlayer2" => vec![("Identity".to_string(), Value::s("Mock Browser"))],
                        _ => vec![],
                    };
                    let _ = conn.reply(&m, vec![Value::dict_sv(props)]);
                }
                ("org.freedesktop.DBus.Properties", "Get") => {
                    let which = m.body[0].as_str().unwrap_or("").to_owned();
                    let prop = m.body[1].as_str().unwrap_or("").to_owned();
                    let all = match which.as_str() {
                        IFACE_PLAYER => {
                            let mut p = Mock::player_props(&s);
                            p.push(("Position".to_string(), Value::I64(s.position)));
                            p
                        }
                        "org.mpris.MediaPlayer2" => vec![("Identity".to_string(), Value::s("Mock Browser"))],
                        _ => vec![],
                    };
                    match all.into_iter().find(|(k, _)| *k == prop) {
                        Some((_, v)) => {
                            let _ = conn.reply(&m, vec![Value::v(v)]);
                        }
                        None => {
                            let _ = conn.reply_error(&m, "org.freedesktop.DBus.Error.UnknownProperty", &prop);
                        }
                    }
                }
                ("org.freedesktop.DBus.Properties", "Set") => {
                    if m.body[1].as_str() == Some("Volume") {
                        s.volume = m.body[2].as_f64().unwrap_or(s.volume);
                        let line = format!("SetVolume {:.2}", s.volume);
                        s.calls.push(line);
                        let _ = conn.reply(&m, vec![]);
                    } else {
                        let _ = conn.reply_error(&m, "org.freedesktop.DBus.Error.PropertyReadOnly", "ro");
                    }
                }
                (IFACE_PLAYER, "PlayPause") => {
                    s.status = if s.status == "Playing" { "Paused".into() } else { "Playing".into() };
                    s.calls.push("PlayPause".into());
                    let _ = conn.reply(&m, vec![]);
                }
                (IFACE_PLAYER, "Play") => {
                    s.status = "Playing".into();
                    s.calls.push("Play".into());
                    let _ = conn.reply(&m, vec![]);
                }
                (IFACE_PLAYER, "Pause") => {
                    s.status = "Paused".into();
                    s.calls.push("Pause".into());
                    let _ = conn.reply(&m, vec![]);
                }
                (IFACE_PLAYER, "SetPosition") => {
                    s.position = m.body[1].as_i64().unwrap_or(0);
                    let line = format!("SetPosition {} {}", m.body[0].as_str().unwrap_or("?"), s.position);
                    s.calls.push(line);
                    let _ = conn.reply(&m, vec![]);
                }
                (IFACE_PLAYER, "Seek") => {
                    s.position += m.body[0].as_i64().unwrap_or(0);
                    s.calls.push(format!("Seek {}", m.body[0].as_i64().unwrap_or(0)));
                    let _ = conn.reply(&m, vec![]);
                }
                (IFACE_PLAYER, member @ ("Next" | "Previous" | "Stop")) => {
                    s.calls.push(member.to_string());
                    let _ = conn.reply(&m, vec![]);
                }
                _ => {
                    let _ = conn.reply_error(&m, "org.freedesktop.DBus.Error.UnknownMethod", &member);
                }
            }
        }
    }

    pub fn update<F: FnOnce(&mut State)>(&self, f: F) {
        f(&mut self.state.lock().unwrap());
    }

    /// Change state and announce it the way real players do.
    pub fn announce<F: FnOnce(&mut State)>(&self, f: F) {
        let s = {
            let mut g = self.state.lock().unwrap();
            f(&mut g);
            g.clone()
        };
        self.conn
            .emit(
                OBJ,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                vec![
                    Value::s(IFACE_PLAYER),
                    Value::dict_sv(vec![
                        ("PlaybackStatus".to_string(), Value::s(s.status.clone())),
                        ("Metadata".to_string(), Mock::metadata(&s)),
                    ]),
                    Value::strings(Vec::<String>::new()),
                ],
            )
            .unwrap();
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
}

