//! MPRIS client over our own D-Bus layer.
//!
//! Browsers (Firefox, LibreWolf, Chromium, Chrome, Brave, Vivaldi, ...) and most
//! media apps register `org.mpris.MediaPlayer2.*` on the session bus. We talk to
//! that instead of anything browser-specific, so it keeps working when you
//! switch tabs, and nothing here polls: changes arrive as signals.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::dbus::{Conn, Error, Message, MsgType, Value};
use crate::model::{pick_player, Snapshot, Status, Track};

pub type Result<T> = std::result::Result<T, Error>;

pub const PREFIX: &str = "org.mpris.MediaPlayer2.";
pub const OBJ: &str = "/org/mpris/MediaPlayer2";
const IFACE_ROOT: &str = "org.mpris.MediaPlayer2";
pub const IFACE_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const CALL_TIMEOUT: Duration = Duration::from_millis(1500);

/// Match rules that cover everything that can change what we show.
pub const MATCH_RULES: [&str; 3] = [
    "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path='/org/mpris/MediaPlayer2'",
    "type='signal',interface='org.mpris.MediaPlayer2.Player',member='Seeked',path='/org/mpris/MediaPlayer2'",
    "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0namespace='org.mpris.MediaPlayer2'",
];

pub fn subscribe(conn: &Conn) -> Result<()> {
    for r in MATCH_RULES {
        conn.add_match(r)?;
    }
    Ok(())
}

/// Does this bus message mean the player display may need refreshing?
pub fn is_relevant(m: &Message) -> bool {
    if m.mtype != MsgType::Signal {
        return false;
    }
    if m.is_signal("org.freedesktop.DBus.Properties", "PropertiesChanged") {
        return m.path.as_deref() == Some(OBJ);
    }
    if m.is_signal(IFACE_PLAYER, "Seeked") {
        return true;
    }
    if m.is_signal("org.freedesktop.DBus", "NameOwnerChanged") {
        return m.body.first().and_then(Value::as_str).is_some_and(|n| n.starts_with(PREFIX));
    }
    false
}

// ---------------------------------------------------------------- parsing

/// `Metadata` (an `a{sv}`) into a `Track`.
pub fn parse_track(meta: &Value) -> Track {
    let string = |k: &str| meta.dict_get(k).and_then(Value::as_str).unwrap_or("").to_owned();

    let artists = match meta.dict_get("xesam:artist") {
        Some(v) if v.as_array().is_some() => v.as_strings(),
        Some(v) => v.as_str().map(|s| vec![s.to_owned()]).unwrap_or_default(),
        None => Vec::new(),
    };
    let art = string("mpris:artUrl");
    Track {
        title: string("xesam:title"),
        artists: artists.into_iter().filter(|a| !a.is_empty()).collect(),
        album: string("xesam:album"),
        art_url: if art.is_empty() { None } else { Some(art) },
        // `x` for well behaved players; as_i64 also accepts u / t / i
        length_us: meta.dict_get("mpris:length").and_then(Value::as_i64).unwrap_or(0),
        track_id: meta.dict_get("mpris:trackid").and_then(Value::as_str).map(str::to_owned),
    }
}

/// Everything read from one player in a single `GetAll`.
#[derive(Default)]
struct Raw {
    status: Option<Status>,
    track: Track,
    volume: Option<f64>,
    position_us: Option<i64>,
    can_seek: bool,
}

fn parse_raw(props: &Value) -> Raw {
    Raw {
        status: props.dict_get("PlaybackStatus").and_then(Value::as_str).map(Status::parse),
        track: props.dict_get("Metadata").map(parse_track).unwrap_or_default(),
        volume: props.dict_get("Volume").and_then(Value::as_f64),
        position_us: props.dict_get("Position").and_then(Value::as_i64),
        can_seek: props.dict_get("CanSeek").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn call(conn: &Conn, bus: &str, iface: &str, member: &str, body: Vec<Value>) -> Result<Message> {
    conn.call_for(Message::call(bus, OBJ, iface, member, body), CALL_TIMEOUT)
}

fn get_all(conn: &Conn, bus: &str, iface: &str) -> Result<Value> {
    let r = call(conn, bus, "org.freedesktop.DBus.Properties", "GetAll", vec![Value::s(iface)])?;
    r.body.into_iter().next().ok_or_else(|| Error::Protocol("empty GetAll".into()))
}

fn get(conn: &Conn, bus: &str, iface: &str, prop: &str) -> Result<Value> {
    let r = call(conn, bus, "org.freedesktop.DBus.Properties", "Get", vec![Value::s(iface), Value::s(prop)])?;
    r.body.into_iter().next().map(|v| v.inner().clone()).ok_or_else(|| Error::Protocol("empty Get".into()))
}

/// All MPRIS bus names currently on the bus, sorted.
pub fn list_players(conn: &Conn) -> Result<Vec<String>> {
    let mut v: Vec<String> = conn
        .list_names()?
        .into_iter()
        // playerctld just mirrors another player; skip the duplicate
        .filter(|n| n.starts_with(PREFIX) && !n.ends_with(".playerctld"))
        .collect();
    v.sort();
    Ok(v)
}

fn identity_from_bus(bus: &str) -> String {
    let tail = bus.trim_start_matches(PREFIX);
    let base = tail.split('.').next().unwrap_or(tail);
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- tracker

/// Remembers what the widgets need to stay steady when a browser misbehaves.
#[derive(Default)]
pub struct Tracker {
    /// The player shown last. Kept while it exists so the display never jumps
    /// away or disappears just because it got paused.
    sticky: Option<String>,
    /// Last non-empty track per player. Browsers briefly report empty metadata
    /// while you switch tabs; we ride that out.
    last_good: HashMap<String, Track>,
    identities: HashMap<String, String>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    pub fn sticky(&self) -> Option<&str> {
        self.sticky.as_deref()
    }

    /// Pick the player to show and read it. `Ok(None)` means no player exists.
    pub fn current(&mut self, conn: &Conn) -> Result<Option<Snapshot>> {
        let names = list_players(conn)?;
        self.last_good.retain(|k, _| names.contains(k));
        self.identities.retain(|k, _| names.contains(k));

        // One GetAll per player. A player that does not answer stays a candidate
        // (as Stopped) so the choice does not flap.
        let raws: Vec<Raw> = names
            .iter()
            .map(|n| get_all(conn, n, IFACE_PLAYER).map(|p| parse_raw(&p)).unwrap_or_default())
            .collect();
        let cands: Vec<(String, Status)> = names
            .iter()
            .zip(&raws)
            .map(|(n, r)| (n.clone(), r.status.unwrap_or(Status::Stopped)))
            .collect();

        let Some(idx) = pick_player(&cands, self.sticky.as_deref()) else {
            self.sticky = None;
            return Ok(None);
        };
        let bus = names[idx].clone();
        let raw = &raws[idx];
        self.sticky = Some(bus.clone());

        let status = raw.status.unwrap_or(Status::Stopped);
        let mut track = raw.track.clone();
        if track.is_empty() && status != Status::Stopped {
            if let Some(t) = self.last_good.get(&bus) {
                track = t.clone();
            }
        } else if !track.is_empty() {
            self.last_good.insert(bus.clone(), track.clone());
        }

        // Position is not always part of GetAll; ask for it on its own.
        let position_us = match raw.position_us {
            Some(p) => p,
            None => get(conn, &bus, IFACE_PLAYER, "Position").ok().and_then(|v| v.as_i64()).unwrap_or(0),
        };
        let identity = self.identity(conn, &bus);
        Ok(Some(Snapshot {
            bus_name: bus,
            identity,
            status,
            track,
            volume: raw.volume.unwrap_or(1.0),
            position_us,
            can_seek: raw.can_seek,
        }))
    }

    fn identity(&mut self, conn: &Conn, bus: &str) -> String {
        if let Some(i) = self.identities.get(bus) {
            return i.clone();
        }
        let id = get(conn, bus, IFACE_ROOT, "Identity")
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| identity_from_bus(bus));
        self.identities.insert(bus.to_owned(), id.clone());
        id
    }
}

// ---------------------------------------------------------------- commands

fn player_call(conn: &Conn, bus: &str, method: &str) -> Result<()> {
    call(conn, bus, IFACE_PLAYER, method, vec![]).map(|_| ())
}

pub fn play_pause(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "PlayPause")
}
pub fn play(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "Play")
}
pub fn pause(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "Pause")
}
pub fn stop(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "Stop")
}
pub fn next(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "Next")
}
pub fn previous(conn: &Conn, bus: &str) -> Result<()> {
    player_call(conn, bus, "Previous")
}

pub fn set_volume(conn: &Conn, bus: &str, volume: f64) -> Result<()> {
    conn.set_prop(bus, OBJ, IFACE_PLAYER, "Volume", Value::F64(volume.clamp(0.0, 1.0)))
}

/// What the player says its volume is right now.
pub fn volume(conn: &Conn, bus: &str) -> Result<f64> {
    get(conn, bus, IFACE_PLAYER, "Volume")?.as_f64().ok_or_else(|| Error::Protocol("Volume is not a number".into()))
}

/// Add `delta` (e.g. 0.05) to the player's volume; returns the new value.
pub fn volume_step(conn: &Conn, bus: &str, delta: f64) -> Result<f64> {
    let now = volume(conn, bus).unwrap_or(1.0);
    let new = (now + delta).clamp(0.0, 1.0);
    set_volume(conn, bus, new)?;
    Ok(new)
}

/// Jump to an absolute position. Uses `SetPosition` when the track id is known
/// and falls back to a relative `Seek`, which every player supports.
pub fn seek_to(conn: &Conn, bus: &str, track_id: Option<&str>, target_us: i64, current_us: i64) -> Result<()> {
    if let Some(id) = track_id {
        if id.starts_with('/') {
            let r = call(conn, bus, IFACE_PLAYER, "SetPosition", vec![Value::path(id), Value::I64(target_us.max(0))]);
            if r.is_ok() {
                return Ok(());
            }
        }
    }
    call(conn, bus, IFACE_PLAYER, "Seek", vec![Value::I64(target_us - current_us)]).map(|_| ())
}

// ---------------------------------------------------------------- client

/// Blocking client for the CLI and the waybar feed: one connection, one tracker.
pub struct Client {
    pub conn: Conn,
    rx: Receiver<Message>,
    pub tracker: Tracker,
}

impl Client {
    pub fn new() -> Result<Client> {
        let (conn, rx) = Conn::session()?;
        Ok(Client { conn, rx, tracker: Tracker::new() })
    }

    /// Connect to an explicit bus address instead of the session bus.
    pub fn new_at(address: &str) -> Result<Client> {
        let (conn, rx) = Conn::connect(address)?;
        Ok(Client { conn, rx, tracker: Tracker::new() })
    }

    pub fn current(&mut self) -> Result<Option<Snapshot>> {
        self.tracker.current(&self.conn)
    }

    pub fn subscribe(&self) -> Result<()> {
        subscribe(&self.conn)
    }

    /// Sleep (no CPU) until something relevant changes or `timeout` passes.
    /// Returns true if something changed. Bursts (a track change sends several
    /// signals) are coalesced into one.
    pub fn wait_for_change(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(m) if is_relevant(&m) => break,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Closed),
            }
        }
        // a short quiet period merges the burst
        loop {
            match self.rx.recv_timeout(Duration::from_millis(40)) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => return Ok(true),
                Err(RecvTimeoutError::Disconnected) => return Ok(true), // reported on the next call
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Value {
        Value::dict_sv(vec![
            ("xesam:title".to_string(), Value::s("夜に駆ける")),
            ("xesam:artist".to_string(), Value::strings(["YOASOBI"])),
            ("xesam:album".to_string(), Value::s("THE BOOK")),
            ("mpris:artUrl".to_string(), Value::s("file:///tmp/a.png")),
            ("mpris:length".to_string(), Value::I64(259_000_000)),
            ("mpris:trackid".to_string(), Value::path("/org/mpris/MediaPlayer2/Track/1")),
        ])
    }

    #[test]
    fn parses_metadata() {
        let t = parse_track(&meta());
        assert_eq!(t.title, "夜に駆ける");
        assert_eq!(t.artists, vec!["YOASOBI"]);
        assert_eq!(t.album, "THE BOOK");
        assert_eq!(t.art_url.as_deref(), Some("file:///tmp/a.png"));
        assert_eq!(t.length_us, 259_000_000);
        assert_eq!(t.track_id.as_deref(), Some("/org/mpris/MediaPlayer2/Track/1"));
    }

    #[test]
    fn tolerates_odd_players() {
        // artist as a plain string, length as unsigned, no art, trackid as string
        let m = Value::dict_sv(vec![
            ("xesam:title".to_string(), Value::s("t")),
            ("xesam:artist".to_string(), Value::s("solo")),
            ("mpris:length".to_string(), Value::U64(5_000_000)),
            ("mpris:trackid".to_string(), Value::s("spotify:track:x")),
        ]);
        let t = parse_track(&m);
        assert_eq!(t.artists, vec!["solo"]);
        assert_eq!(t.length_us, 5_000_000);
        assert_eq!(t.art_url, None);
        assert_eq!(t.track_id.as_deref(), Some("spotify:track:x"));
        // empty and missing metadata
        assert!(parse_track(&Value::dict_sv(vec![])).is_empty());
        assert!(parse_track(&Value::I32(3)).is_empty());
        // empty artist strings are dropped
        let m = Value::dict_sv(vec![("xesam:artist".to_string(), Value::strings([""]))]);
        assert!(parse_track(&m).artists.is_empty());
    }

    #[test]
    fn identity_fallback_from_bus_name() {
        assert_eq!(identity_from_bus("org.mpris.MediaPlayer2.firefox.instance_1_42"), "Firefox");
        assert_eq!(identity_from_bus("org.mpris.MediaPlayer2.spotify"), "Spotify");
    }

    #[test]
    fn relevance_filter() {
        let sig = |iface: &str, member: &str, path: &str, body: Vec<Value>| Message::signal(path, iface, member, body);
        assert!(is_relevant(&sig("org.freedesktop.DBus.Properties", "PropertiesChanged", OBJ, vec![])));
        assert!(!is_relevant(&sig("org.freedesktop.DBus.Properties", "PropertiesChanged", "/other", vec![])));
        assert!(is_relevant(&sig(IFACE_PLAYER, "Seeked", OBJ, vec![Value::I64(1)])));
        assert!(is_relevant(&sig(
            "org.freedesktop.DBus",
            "NameOwnerChanged",
            "/org/freedesktop/DBus",
            vec![Value::s("org.mpris.MediaPlayer2.x"), Value::s(""), Value::s(":1.2")]
        )));
        assert!(!is_relevant(&sig(
            "org.freedesktop.DBus",
            "NameOwnerChanged",
            "/org/freedesktop/DBus",
            vec![Value::s("org.other"), Value::s(""), Value::s(":1.2")]
        )));
    }
}
