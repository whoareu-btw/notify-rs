//! Pure logic: no D-Bus, no GUI, std only. Everything here is unit-tested.

use std::time::Instant;

// Font Awesome codepoints. They sit in the BMP private-use area and are
// identical in Nerd Font v2 and v3, so they work with any patched font.
pub const ICON_PLAY: char = '\u{f04b}'; //  nf-fa-play
pub const ICON_PAUSE: char = '\u{f03e4}'; // 󰏤 nf-md-pause (Nerd Fonts v3)
pub const ICON_PREV: char = '\u{f048}';
pub const ICON_NEXT: char = '\u{f051}';
pub const ICON_VOL_HIGH: char = '\u{f028}';
pub const ICON_VOL_LOW: char = '\u{f027}';
pub const ICON_VOL_OFF: char = '\u{f026}';
pub const ICON_MUSIC: char = '\u{f001}';
// The waybar bell uses Nerd Fonts v3 Material Design icons. Each one is always
// followed by a space ("icon<space>"): those glyphs are wider than their advance
// and waybar clips the last one at the edge of the module without it.
pub const ICON_BELL: char = '\u{f009a}'; // 󰂚 nf-md-bell
pub const ICON_BELL_NEW: char = '\u{f116b}'; // 󱅫 nf-md-bell_badge
pub const ICON_BELL_OFF: char = '\u{f009b}'; // 󰂛 nf-md-bell_off

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

impl Status {
    pub fn parse(s: &str) -> Status {
        match s {
            "Playing" => Status::Playing,
            "Paused" => Status::Paused,
            _ => Status::Stopped,
        }
    }

    pub fn class(self) -> &'static str {
        match self {
            Status::Playing => "playing",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub art_url: Option<String>,
    pub length_us: i64,
    pub track_id: Option<String>,
}

impl Track {
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artists.is_empty() && self.album.is_empty()
    }

    pub fn artist_line(&self) -> String {
        self.artists.join(", ")
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Full bus name, e.g. `org.mpris.MediaPlayer2.firefox.instance_1_42`
    pub bus_name: String,
    /// Human name, e.g. `Firefox`
    pub identity: String,
    pub status: Status,
    pub track: Track,
    /// 0.0 ..= 1.0 (players may report more, we clamp for display only)
    pub volume: f64,
    pub position_us: i64,
    pub can_seek: bool,
}

/// Choose which player to show. `candidates` is (bus name, status).
///
/// Rules, in order:
/// 1. Something is playing: keep the sticky player if it is one of them,
///    otherwise take the first playing one.
/// 2. Nothing playing but the sticky player still exists: stay on it.
///    This is what stops the widget from vanishing or jumping around
///    when a tab is paused or you switch tabs.
/// 3. Otherwise the first paused player, then the first of anything.
pub fn pick_player(candidates: &[(String, Status)], sticky: Option<&str>) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let sticky_idx = sticky.and_then(|s| candidates.iter().position(|(n, _)| n == s));
    let first_playing = candidates.iter().position(|(_, st)| *st == Status::Playing);

    if let Some(p) = first_playing {
        if let Some(i) = sticky_idx {
            if candidates[i].1 == Status::Playing {
                return Some(i);
            }
        }
        return Some(p);
    }
    if let Some(i) = sticky_idx {
        return Some(i);
    }
    candidates
        .iter()
        .position(|(_, st)| *st == Status::Paused)
        .or(Some(0))
}

/// `m:ss`, or `h:mm:ss` from one hour up. Negative values show as zero.
pub fn fmt_time(us: i64) -> String {
    let total = (us.max(0) / 1_000_000) as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Cut to `max` characters (not bytes), adding an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Escape for Pango markup (waybar parses module text and tooltips as markup).
pub fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape for a JSON string body (no surrounding quotes).
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn volume_icon(volume: f64) -> char {
    if volume <= 0.001 {
        ICON_VOL_OFF
    } else if volume < 0.5 {
        ICON_VOL_LOW
    } else {
        ICON_VOL_HIGH
    }
}

/// One line of waybar custom-module JSON. `None` means no player at all:
/// empty text, which waybar hides.
///
/// `icon_only` shows just the play/pause glyph in the bar; the tooltip still
/// carries title, artist, album and source.
pub fn waybar_json(snap: Option<&Snapshot>, max_len: usize, icon_only: bool) -> String {
    let snap = match snap {
        Some(s) => s,
        None => return r#"{"text":"","tooltip":"","class":"none","alt":"none","percentage":0}"#.into(),
    };
    let t = &snap.track;
    let icon = match snap.status {
        Status::Playing => ICON_PLAY,
        _ => ICON_PAUSE,
    };
    let label = match (t.artists.is_empty(), t.title.is_empty()) {
        (false, false) => format!("{} - {}", t.artist_line(), t.title),
        (true, false) => t.title.clone(),
        (false, true) => t.artist_line(),
        (true, true) => snap.identity.clone(),
    };
    let text = if icon_only {
        icon.to_string()
    } else {
        format!("{icon} {}", escape_markup(&truncate(&label, max_len)))
    };

    let mut tip = String::new();
    if !t.title.is_empty() {
        tip.push_str(&format!("<b>{}</b>", escape_markup(&t.title)));
    }
    if !t.artists.is_empty() {
        tip.push_str(&format!("\n{}", escape_markup(&t.artist_line())));
    }
    if !t.album.is_empty() {
        tip.push_str(&format!("\n<i>{}</i>", escape_markup(&t.album)));
    }
    if !snap.identity.is_empty() {
        tip.push_str(&format!("\n{}", escape_markup(&snap.identity)));
    }
    let tip = tip.trim_start_matches('\n').to_string();

    let pct = (snap.volume.clamp(0.0, 1.0) * 100.0).round() as i64;
    format!(
        r#"{{"text":"{}","tooltip":"{}","class":"{}","alt":"{}","percentage":{}}}"#,
        json_escape(&text),
        json_escape(&tip),
        snap.status.class(),
        snap.status.class(),
        pct
    )
}

/// One line of waybar JSON for the notification bell.
/// `state` is (panel open, Do Not Disturb, count); `None` means the daemon is not running.
/// With `hide_empty` the module disappears (empty text) while there is nothing to show.
pub fn bell_json(state: Option<(bool, bool, u32)>, hide_empty: bool) -> String {
    let Some((open, dnd, count)) = state else {
        return r#"{"text":"","tooltip":"notify-rs is not running","class":"offline","alt":"offline"}"#.into();
    };
    let class = if dnd {
        "dnd"
    } else if count > 0 {
        "has"
    } else {
        "empty"
    };
    let icon = if dnd {
        ICON_BELL_OFF
    } else if count > 0 {
        ICON_BELL_NEW
    } else {
        ICON_BELL
    };
    let text = if hide_empty && !dnd && count == 0 {
        String::new()
    } else {
        // always "icon<space>"; the count is in the tooltip and the `percentage` field
        format!("{icon} ")
    };
    let tip = match (dnd, count) {
        (true, 0) => "Do not disturb".to_string(),
        (true, n) => format!("Do not disturb, {n} notification{}", if n == 1 { "" } else { "s" }),
        (false, 0) => "No notifications".to_string(),
        (false, n) => format!("{n} notification{}", if n == 1 { "" } else { "s" }),
    };
    let class = if open { format!("{class} open") } else { class.to_string() };
    format!(
        r#"{{"text":"{}","tooltip":"{}","class":"{}","alt":"{}","percentage":{}}}"#,
        json_escape(&text),
        json_escape(&tip),
        class,
        if dnd { "dnd" } else { "on" },
        count.min(100)
    )
}

/// Turn `file:///a%20b.png` into `/a b.png`. Returns None for other schemes.
pub fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file://localhost/x` and `file:///x` both mean /x
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// `--anchor` text -> (top, bottom, left, right) edges to stick to.
/// Plain `top` / `bottom` mean centred. Unknown text falls back to `top`
/// (centred under a top bar) rather than doing something surprising.
pub fn parse_anchor(a: &str) -> (bool, bool, bool, bool) {
    match a {
        "top-left" => (true, false, true, false),
        "top-right" => (true, false, false, true),
        "bottom" => (false, true, false, false),
        "bottom-left" => (false, true, true, false),
        "bottom-right" => (false, true, false, true),
        _ => (true, false, false, false),
    }
}

/// `~/.cache/notify-rs` (honours `XDG_CACHE_HOME`): where downloaded covers live.
pub fn cache_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    Some(base.join("notify-rs"))
}

/// Stable, filename-safe hash (FNV-1a) used for the album-art cache.
pub fn cache_key(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Position that advances locally so we never poll D-Bus once a second.
/// Re-synced whenever the player reports a change or a seek.
#[derive(Clone, Copy, Debug)]
pub struct PositionClock {
    base_us: i64,
    at: Instant,
    ticking: bool,
}

impl PositionClock {
    pub fn new(base_us: i64, playing: bool) -> Self {
        PositionClock { base_us, at: Instant::now(), ticking: playing }
    }

    pub fn now_us(&self, length_us: i64) -> i64 {
        let mut p = self.base_us;
        if self.ticking {
            p += self.at.elapsed().as_micros() as i64;
        }
        if length_us > 0 {
            p = p.min(length_us);
        }
        p.max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(list: &[(&str, Status)]) -> Vec<(String, Status)> {
        list.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn pick_none() {
        assert_eq!(pick_player(&[], None), None);
    }

    #[test]
    fn pick_prefers_playing() {
        let l = c(&[("a", Status::Paused), ("b", Status::Playing)]);
        assert_eq!(pick_player(&l, None), Some(1));
    }

    #[test]
    fn pick_keeps_sticky_when_both_playing() {
        let l = c(&[("a", Status::Playing), ("b", Status::Playing)]);
        assert_eq!(pick_player(&l, Some("b")), Some(1));
    }

    #[test]
    fn pick_switches_to_new_playing_player() {
        let l = c(&[("a", Status::Paused), ("b", Status::Playing)]);
        assert_eq!(pick_player(&l, Some("a")), Some(1));
    }

    #[test]
    fn pick_stays_on_paused_sticky() {
        // the "tab switch" case: nothing playing, we must not jump or vanish
        let l = c(&[("a", Status::Stopped), ("b", Status::Paused)]);
        assert_eq!(pick_player(&l, Some("a")), Some(0));
    }

    #[test]
    fn pick_falls_back_to_paused_then_first() {
        let l = c(&[("a", Status::Stopped), ("b", Status::Paused)]);
        assert_eq!(pick_player(&l, Some("gone")), Some(1));
        let l = c(&[("a", Status::Stopped)]);
        assert_eq!(pick_player(&l, None), Some(0));
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(0), "0:00");
        assert_eq!(fmt_time(65_000_000), "1:05");
        assert_eq!(fmt_time(3_725_000_000), "1:02:05");
        assert_eq!(fmt_time(-5), "0:00");
    }

    #[test]
    fn truncate_counts_chars() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello\u{2026}");
        assert_eq!(truncate("日本語のタイトル", 4), "日本語\u{2026}");
        assert_eq!(truncate("abc", 0), "abc");
    }

    #[test]
    fn escapes() {
        assert_eq!(escape_markup("A & B <c>"), "A &amp; B &lt;c&gt;");
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    fn snap(status: Status, title: &str, artist: &str) -> Snapshot {
        Snapshot {
            bus_name: "org.mpris.MediaPlayer2.firefox".into(),
            identity: "Firefox".into(),
            status,
            track: Track {
                title: title.into(),
                artists: if artist.is_empty() { vec![] } else { vec![artist.into()] },
                album: "Alb & Co".into(),
                ..Default::default()
            },
            volume: 0.756,
            position_us: 0,
            can_seek: true,
        }
    }

    #[test]
    fn waybar_none() {
        let j = waybar_json(None, 40, false);
        assert!(j.contains(r#""text":"""#) && j.contains(r#""class":"none""#));
    }

    #[test]
    fn waybar_playing() {
        let j = waybar_json(Some(&snap(Status::Playing, "Song <1>", "Band")), 40, false);
        assert!(j.starts_with("{\"text\":\""));
        assert!(j.contains("Band - Song &lt;1&gt;"));
        assert!(j.contains(r#""class":"playing""#));
        assert!(j.contains(r#""percentage":76"#));
        assert!(j.contains("Alb &amp; Co"));
        assert!(!j.contains('\n'));
    }

    #[test]
    fn waybar_title_only_and_paused_icon() {
        let j = waybar_json(Some(&snap(Status::Paused, "Only", "")), 40, false);
        assert!(j.contains("Only") && !j.contains(" - "));
        assert!(j.contains(ICON_PAUSE));
        assert!(j.contains(r#""class":"paused""#));
    }

    #[test]
    fn waybar_truncates() {
        let j = waybar_json(Some(&snap(Status::Playing, "x".repeat(100).as_str(), "")), 10, false);
        assert!(j.contains('\u{2026}'));
    }

    #[test]
    fn waybar_icon_only_keeps_tooltip() {
        let j = waybar_json(Some(&snap(Status::Playing, "My Song", "Band")), 40, true);
        // bar text is just the glyph...
        assert!(j.starts_with(&format!("{{\"text\":\"{}\",", ICON_PLAY)));
        assert!(!j.contains("\"text\":\"\u{f04b} "));
        // ...while the tooltip still names the track
        let tip = j.split("\"tooltip\":").nth(1).unwrap();
        assert!(tip.contains("My Song") && tip.contains("Band"));
        let paused = waybar_json(Some(&snap(Status::Paused, "My Song", "Band")), 40, true);
        assert!(paused.starts_with(&format!("{{\"text\":\"{}\",", ICON_PAUSE)));
    }

    #[test]
    fn uri_decoding() {
        assert_eq!(file_uri_to_path("file:///tmp/a%20b.png").as_deref(), Some("/tmp/a b.png"));
        assert_eq!(file_uri_to_path("file://localhost/x/y.jpg").as_deref(), Some("/x/y.jpg"));
        assert_eq!(file_uri_to_path("https://x/y.jpg"), None);
        assert_eq!(file_uri_to_path("file:///100%").as_deref(), Some("/100%"));
        assert_eq!(file_uri_to_path("file:///a%2").as_deref(), Some("/a%2"));
    }

    #[test]
    fn cache_key_stable() {
        assert_eq!(cache_key("abc"), cache_key("abc"));
        assert_ne!(cache_key("abc"), cache_key("abd"));
        assert_eq!(cache_key("abc").len(), 16);
    }

    #[test]
    fn clock_ticks_only_when_playing() {
        let paused = PositionClock::new(5_000_000, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(paused.now_us(0), 5_000_000);
        let playing = PositionClock::new(5_000_000, true);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(playing.now_us(0) > 5_000_000);
        assert_eq!(playing.now_us(5_000_000), 5_000_000);
    }

    #[test]
    fn anchors() {
        assert_eq!(parse_anchor("top-left"), (true, false, true, false));
        assert_eq!(parse_anchor("top-right"), (true, false, false, true));
        assert_eq!(parse_anchor("top"), (true, false, false, false));
        assert_eq!(parse_anchor("bottom"), (false, true, false, false));
        assert_eq!(parse_anchor("bottom-right"), (false, true, false, true));
        assert_eq!(parse_anchor("nonsense"), (true, false, false, false));
    }

    #[test]
    fn play_and_pause_icons_are_distinct_nerd_glyphs() {
        assert_eq!(ICON_PLAY as u32, 0xf04b);
        assert_eq!(ICON_PAUSE as u32, 0xf03e4);
    }

    #[test]
    fn bell_states() {
        let j = bell_json(None, false);
        assert!(j.contains(r#""class":"offline""#) && j.contains(r#""text":"""#));
        let j = bell_json(Some((false, false, 0)), false);
        assert!(j.contains(r#""class":"empty""#) && j.contains("No notifications"));
        assert!(bell_json(Some((false, false, 0)), true).starts_with(r#"{"text":"","#), "hidden when empty");
        let j = bell_json(Some((false, false, 3)), true);
        assert!(j.contains(r#""class":"has""#) && j.contains("3 notifications"));
        assert!(bell_json(Some((false, false, 1)), false).contains("1 notification\""));
        let j = bell_json(Some((true, true, 2)), true);
        assert!(j.contains("dnd open") && j.contains("Do not disturb, 2 notifications"));
        assert!(!bell_json(Some((false, true, 0)), true).starts_with(r#"{"text":"","#), "dnd stays visible");
    }

    /// The exact text waybar receives: Nerd Font icon, then one regular space.
    #[test]
    fn bell_text_is_icon_then_space() {
        let text = |state, hide| {
            let j = bell_json(Some(state), hide);
            let start = j.find("\"text\":\"").unwrap() + 8;
            j[start..j[start..].find('"').unwrap() + start].to_string()
        };
        // nothing waiting: U+F009A + " "
        assert_eq!(text((false, false, 0), false), "\u{f009a} ");
        // something waiting: U+F116B + " ", no count in the text
        assert_eq!(text((false, false, 1), false), "\u{f116b} ");
        assert_eq!(text((true, false, 12), false), "\u{f116b} ");
        // Do Not Disturb keeps the same shape
        assert_eq!(text((false, true, 0), false), "\u{f009b} ");
        assert_eq!(text((false, true, 4), false), "\u{f009b} ");
        // the space is a plain U+0020, not a no-break or thin space
        assert!(text((false, false, 0), false).ends_with('\u{20}'));
        // and the constants are the ones that were asked for
        assert_eq!((ICON_BELL, ICON_BELL_NEW, ICON_BELL_OFF), ('\u{f009a}', '\u{f116b}', '\u{f009b}'));
    }

    #[test]
    fn volume_icons() {
        assert_eq!(volume_icon(0.0), ICON_VOL_OFF);
        assert_eq!(volume_icon(0.3), ICON_VOL_LOW);
        assert_eq!(volume_icon(0.9), ICON_VOL_HIGH);
    }
}
