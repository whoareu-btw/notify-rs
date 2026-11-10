//! Notification center state machine (freedesktop Notifications 1.2 semantics).
//!
//! Pure logic: no D-Bus, no drawing, time is passed in. `service.rs` maps D-Bus
//! calls onto this and `ui` draws it.
//!
//! Behaviour follows GNOME Shell where the spec is silent:
//! * a notification stays in the list until dismissed; its pop-up (toast) just
//!   times out, and `NotificationClosed` is only sent when it leaves the list;
//! * `transient` notifications leave the list when their toast times out;
//! * Do Not Disturb hides toasts, except for critical notifications;
//! * while the panel is open no toasts are shown.

use std::time::{Duration, Instant};

use crate::dbus::Value;

pub const REASON_EXPIRED: u32 = 1;
pub const REASON_DISMISSED: u32 = 2;
pub const REASON_CLOSED: u32 = 3;
pub const REASON_UNDEFINED: u32 = 4;

/// Oldest notifications are dropped beyond this many.
pub const MAX_ITEMS: usize = 100;
/// At most this many toasts are drawn at once.
pub const MAX_TOASTS: usize = 3;
const MAX_IMAGE_SIDE: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// Straight (non-premultiplied) RGBA8.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Toast {
    Hidden,
    Until(Instant),
    /// Shown until the user acts (critical, or expire_timeout 0).
    Sticky,
}

#[derive(Clone, Debug)]
pub struct NotifyReq {
    pub app_name: String,
    pub replaces_id: u32,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    /// (key, label) pairs
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    pub category: String,
    pub desktop_entry: String,
    pub image: Option<Image>,
    pub image_path: Option<String>,
    pub resident: bool,
    pub transient: bool,
    pub progress: Option<i32>,
    pub sync_tag: Option<String>,
    pub timeout_ms: i32,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    pub category: String,
    pub desktop_entry: String,
    pub image: Option<Image>,
    pub image_path: Option<String>,
    pub resident: bool,
    pub transient: bool,
    pub progress: Option<i32>,
    pub sync_tag: Option<String>,
    /// How long the toast stays; None = until dismissed.
    pub timeout: Option<Duration>,
    /// Seconds since the Unix epoch when it arrived (for "5 min ago").
    pub created_unix: i64,
    pub toast: Toast,
}

impl Notification {
    /// Actions shown as buttons (everything except the click-the-body "default").
    pub fn button_actions(&self) -> impl Iterator<Item = &(String, String)> {
        self.actions.iter().filter(|(k, _)| k != "default")
    }
    pub fn has_default_action(&self) -> bool {
        self.actions.iter().any(|(k, _)| k == "default")
    }
}

/// What the daemon must tell applications over D-Bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Closed(u32, u32),
    Action(u32, String),
}

pub fn effective_timeout(requested_ms: i32, urgency: Urgency) -> Option<Duration> {
    match requested_ms {
        0 => None,
        t if t > 0 => Some(Duration::from_millis(t.max(1000) as u64)),
        _ => match urgency {
            Urgency::Low => Some(Duration::from_secs(4)),
            Urgency::Normal => Some(Duration::from_secs(6)),
            Urgency::Critical => None,
        },
    }
}

// ---------------------------------------------------------------- parsing

/// Turn the arguments of `Notify` (`susssasa{sv}i`) into a request.
pub fn parse_notify(body: &[Value]) -> Result<NotifyReq, String> {
    if crate::dbus::value::signature_of(body) != "susssasa{sv}i" {
        return Err(format!("expected susssasa{{sv}}i, got {}", crate::dbus::value::signature_of(body)));
    }
    let s = |i: usize| body[i].as_str().unwrap_or("").to_owned();

    let flat = body[5].as_strings();
    let actions: Vec<(String, String)> =
        flat.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();

    let hints = &body[6];
    let hs = |k: &str| hints.dict_get(k).and_then(Value::as_str).map(str::to_owned);
    let hb = |k: &str| hints.dict_get(k).and_then(Value::as_bool).unwrap_or(false);

    let urgency = match hints.dict_get("urgency").and_then(Value::as_i64) {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    };

    // image-data has had three names over the years
    let image = ["image-data", "image_data", "icon_data"]
        .iter()
        .find_map(|k| hints.dict_get(k))
        .and_then(parse_image);

    let sync_tag = hs("x-canonical-private-synchronous")
        .or_else(|| hs("synchronous"))
        .or_else(|| hs("x-dunst-stack-tag"))
        .filter(|t| !t.is_empty());

    Ok(NotifyReq {
        app_name: s(0),
        replaces_id: body[1].as_u32().unwrap_or(0),
        app_icon: s(2),
        summary: s(3),
        body: s(4),
        actions,
        urgency,
        category: hs("category").unwrap_or_default(),
        desktop_entry: hs("desktop-entry").unwrap_or_default(),
        image,
        image_path: hs("image-path").or_else(|| hs("image_path")).filter(|p| !p.is_empty()),
        resident: hb("resident"),
        transient: hb("transient"),
        progress: hints.dict_get("value").and_then(Value::as_i64).map(|v| v.clamp(0, 100) as i32),
        sync_tag,
        timeout_ms: body[7].as_i64().unwrap_or(-1) as i32,
    })
}

/// `(iiibiiay)`: width, height, rowstride, has_alpha, bits per sample, channels, data.
fn parse_image(v: &Value) -> Option<Image> {
    let f = v.as_struct()?;
    if f.len() != 7 {
        return None;
    }
    let w = f[0].as_i64()?;
    let h = f[1].as_i64()?;
    let stride = f[2].as_i64()?;
    let alpha = f[3].as_bool()?;
    let bits = f[4].as_i64()?;
    let ch = f[5].as_i64()?;
    let data: Vec<u8> = f[6].as_array()?.iter().filter_map(|b| if let Value::Byte(b) = b { Some(*b) } else { None }).collect();

    if bits != 8 || !(ch == 3 || ch == 4) || (ch == 4) != alpha {
        return None;
    }
    if w <= 0 || h <= 0 || w as u32 > MAX_IMAGE_SIDE || h as u32 > MAX_IMAGE_SIDE {
        return None;
    }
    let (w, h, stride, ch) = (w as usize, h as usize, stride as usize, ch as usize);
    if stride < w * ch || data.len() < stride * (h - 1) + w * ch {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &data[y * stride..y * stride + w * ch];
        for px in row.chunks_exact(ch) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], if ch == 4 { px[3] } else { 255 }]);
        }
    }
    Some(Image { w: w as u32, h: h as u32, rgba })
}

/// Notification body markup (`<b> <i> <u> <a href>`, entities) to plain text.
pub fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // only treat it as a tag if a '>' follows reasonably soon and it looks like one
                let rest: String = chars.clone().take(64).collect();
                let looks_like_tag = rest
                    .find('>')
                    .map(|end| {
                        let inner = &rest[..end];
                        let first = inner.trim_start_matches('/').chars().next();
                        first.is_some_and(|f| f.is_ascii_alphabetic())
                    })
                    .unwrap_or(false);
                if looks_like_tag {
                    let end = rest.find('>').unwrap();
                    let name: String = rest[..end]
                        .trim_start_matches('/')
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if name == "br" {
                        out.push('\n');
                    }
                    for _ in 0..rest[..=end].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('<');
                }
            }
            '&' => {
                let rest: String = chars.clone().take(10).collect();
                if let Some(end) = rest.find(';') {
                    let ent = &rest[..end];
                    let rep = match ent {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => ent
                            .strip_prefix('#')
                            .and_then(|n| {
                                if let Some(h) = n.strip_prefix(['x', 'X']) {
                                    u32::from_str_radix(h, 16).ok()
                                } else {
                                    n.parse().ok()
                                }
                            })
                            .and_then(char::from_u32),
                    };
                    if let Some(r) = rep {
                        out.push(r);
                        for _ in 0..rest[..=end].chars().count() {
                            chars.next();
                        }
                        continue;
                    }
                }
                out.push('&');
            }
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- the center

pub struct Center {
    /// oldest first
    items: Vec<Notification>,
    next_id: u32,
    dnd: bool,
    panel_open: bool,
}

impl Default for Center {
    fn default() -> Self {
        Center::new()
    }
}

impl Center {
    pub fn new() -> Center {
        Center { items: Vec::new(), next_id: 1, dnd: false, panel_open: false }
    }

    pub fn dnd(&self) -> bool {
        self.dnd
    }
    pub fn panel_open(&self) -> bool {
        self.panel_open
    }
    pub fn count(&self) -> usize {
        self.items.len()
    }
    /// Newest first, which is how the panel lists them.
    pub fn items_newest_first(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter().rev()
    }
    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.items.iter().find(|n| n.id == id)
    }

    fn alloc_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).unwrap_or(1);
            if id != 0 && self.get(id).is_none() {
                return id;
            }
        }
    }

    /// `Notify`. Returns the id and anything to announce over D-Bus.
    pub fn notify(&mut self, req: NotifyReq, now: Instant, now_unix: i64) -> (u32, Vec<Out>) {
        let mut outs = Vec::new();

        // Which existing notification (if any) does this replace?
        let existing = if req.replaces_id != 0 {
            self.items.iter().position(|n| n.id == req.replaces_id)
        } else if let Some(tag) = &req.sync_tag {
            self.items.iter().position(|n| n.sync_tag.as_ref() == Some(tag) && n.app_name == req.app_name)
        } else {
            None
        };
        let id = match existing {
            Some(i) => self.items.remove(i).id,
            None if req.replaces_id != 0 => req.replaces_id, // app asked for this id: honour it
            None => self.alloc_id(),
        };

        let timeout = effective_timeout(req.timeout_ms, req.urgency);
        let show = !self.panel_open && (!self.dnd || req.urgency == Urgency::Critical);
        let toast = match (show, timeout) {
            (false, _) => Toast::Hidden,
            (true, Some(d)) => Toast::Until(now + d),
            (true, None) => Toast::Sticky,
        };
        self.items.push(Notification {
            id,
            app_name: req.app_name,
            app_icon: req.app_icon,
            summary: req.summary,
            body: req.body,
            actions: req.actions,
            urgency: req.urgency,
            category: req.category,
            desktop_entry: req.desktop_entry,
            image: req.image,
            image_path: req.image_path,
            resident: req.resident,
            transient: req.transient,
            progress: req.progress,
            sync_tag: req.sync_tag,
            timeout,
            created_unix: now_unix,
            toast,
        });

        while self.items.len() > MAX_ITEMS {
            let old = self.items.remove(0);
            outs.push(Out::Closed(old.id, REASON_UNDEFINED));
        }
        (id, outs)
    }

    fn remove(&mut self, id: u32, reason: u32) -> Vec<Out> {
        match self.items.iter().position(|n| n.id == id) {
            Some(i) => {
                self.items.remove(i);
                vec![Out::Closed(id, reason)]
            }
            None => vec![],
        }
    }

    /// `CloseNotification` from the application.
    pub fn close(&mut self, id: u32) -> Vec<Out> {
        self.remove(id, REASON_CLOSED)
    }

    /// The user dismissed it.
    pub fn dismiss(&mut self, id: u32) -> Vec<Out> {
        self.remove(id, REASON_DISMISSED)
    }

    pub fn clear_all(&mut self) -> Vec<Out> {
        self.items.drain(..).rev().map(|n| Out::Closed(n.id, REASON_DISMISSED)).collect()
    }

    /// The user hid just the toast (clicked its close button); it stays in the panel.
    pub fn hide_toast(&mut self, id: u32) {
        if let Some(n) = self.items.iter_mut().find(|n| n.id == id) {
            n.toast = Toast::Hidden;
        }
    }

    /// The user clicked an action (or the body, with key "default").
    /// Unless the notification is `resident` it is dismissed afterwards.
    pub fn activate(&mut self, id: u32, key: &str) -> Vec<Out> {
        let Some(n) = self.get(id) else { return vec![] };
        if !n.actions.iter().any(|(k, _)| k == key) {
            return vec![];
        }
        let resident = n.resident;
        let mut outs = vec![Out::Action(id, key.to_owned())];
        if resident {
            self.hide_toast(id);
        } else {
            outs.extend(self.dismiss(id));
        }
        outs
    }

    /// Re-arm a toast's timer (used while the pointer hovers it).
    pub fn postpone_toast(&mut self, id: u32, now: Instant) {
        if let Some(n) = self.items.iter_mut().find(|n| n.id == id) {
            if let (Toast::Until(_), Some(d)) = (&n.toast, n.timeout) {
                n.toast = Toast::Until(now + d);
            }
        }
    }

    pub fn set_dnd(&mut self, on: bool) {
        self.dnd = on;
        if on {
            for n in &mut self.items {
                if n.urgency != Urgency::Critical {
                    n.toast = Toast::Hidden;
                }
            }
        }
    }

    pub fn set_panel_open(&mut self, open: bool) {
        self.panel_open = open;
        if open {
            for n in &mut self.items {
                n.toast = Toast::Hidden;
            }
        }
    }

    /// Visible toasts, newest first, at most `MAX_TOASTS`.
    pub fn toasts(&self) -> Vec<&Notification> {
        self.items.iter().rev().filter(|n| n.toast != Toast::Hidden).take(MAX_TOASTS).collect()
    }

    /// Expire toasts whose time is up. Returns what to announce and when to call again.
    pub fn tick(&mut self, now: Instant) -> (Vec<Out>, Option<Instant>) {
        let mut outs = Vec::new();
        let mut gone = Vec::new();
        for n in &mut self.items {
            if let Toast::Until(t) = n.toast {
                if t <= now {
                    n.toast = Toast::Hidden;
                    if n.transient {
                        gone.push(n.id);
                    }
                }
            }
        }
        for id in gone {
            outs.extend(self.remove(id, REASON_EXPIRED));
        }
        let next = self
            .items
            .iter()
            .filter_map(|n| if let Toast::Until(t) = n.toast { Some(t) } else { None })
            .min();
        (outs, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(app: &str, summary: &str) -> NotifyReq {
        NotifyReq {
            app_name: app.into(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: summary.into(),
            body: String::new(),
            actions: vec![],
            urgency: Urgency::Normal,
            category: String::new(),
            desktop_entry: String::new(),
            image: None,
            image_path: None,
            resident: false,
            transient: false,
            progress: None,
            sync_tag: None,
            timeout_ms: -1,
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn ids_are_unique_and_replace_keeps_id() {
        let mut c = Center::new();
        let (a, _) = c.notify(req("x", "one"), t0(), 100);
        let (b, _) = c.notify(req("x", "two"), t0(), 101);
        assert_ne!(a, b);
        let mut r = req("x", "one again");
        r.replaces_id = a;
        let (a2, outs) = c.notify(r, t0(), 102);
        assert_eq!(a2, a);
        assert!(outs.is_empty(), "replacing sends no close signal");
        assert_eq!(c.count(), 2);
        assert_eq!(c.items_newest_first().next().unwrap().summary, "one again", "moved to the top");
    }

    #[test]
    fn replacing_an_unknown_id_uses_that_id_and_later_ids_skip_it() {
        let mut c = Center::new();
        let mut r = req("x", "a");
        r.replaces_id = 2;
        let (id, _) = c.notify(r, t0(), 0);
        assert_eq!(id, 2);
        let (a, _) = c.notify(req("x", "b"), t0(), 0);
        let (b, _) = c.notify(req("x", "c"), t0(), 0);
        assert_eq!(a, 1);
        assert_eq!(b, 3, "2 is taken");
    }

    #[test]
    fn sync_tag_replaces_in_place_per_app() {
        let mut c = Center::new();
        let mut r = req("volume", "50%");
        r.sync_tag = Some("vol".into());
        let (a, _) = c.notify(r.clone(), t0(), 0);
        r.summary = "60%".into();
        let (b, _) = c.notify(r.clone(), t0(), 0);
        assert_eq!(a, b);
        assert_eq!(c.count(), 1);
        r.app_name = "other".into();
        let (d, _) = c.notify(r, t0(), 0);
        assert_ne!(d, a, "same tag from another app is separate");
    }

    #[test]
    fn toast_timing_and_default_timeouts() {
        let mut c = Center::new();
        let now = t0();
        let (id, _) = c.notify(req("x", "hi"), now, 0);
        assert_eq!(c.toasts().len(), 1);
        let (outs, next) = c.tick(now + Duration::from_secs(3));
        assert!(outs.is_empty());
        assert_eq!(next, Some(now + Duration::from_secs(6)));
        assert_eq!(c.toasts().len(), 1);
        let (outs, next) = c.tick(now + Duration::from_secs(7));
        assert!(outs.is_empty(), "no close signal: it stays in the list");
        assert_eq!(next, None);
        assert!(c.toasts().is_empty());
        assert!(c.get(id).is_some());
        assert_eq!(effective_timeout(0, Urgency::Normal), None);
        assert_eq!(effective_timeout(-1, Urgency::Critical), None);
        assert_eq!(effective_timeout(2500, Urgency::Low), Some(Duration::from_millis(2500)));
        assert_eq!(effective_timeout(10, Urgency::Low), Some(Duration::from_secs(1)), "floor");
    }

    #[test]
    fn transient_leaves_the_list_when_its_toast_expires() {
        let mut c = Center::new();
        let now = t0();
        let mut r = req("x", "osd");
        r.transient = true;
        let (id, _) = c.notify(r, now, 0);
        let (outs, _) = c.tick(now + Duration::from_secs(10));
        assert_eq!(outs, vec![Out::Closed(id, REASON_EXPIRED)]);
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn critical_is_sticky_and_survives_dnd() {
        let mut c = Center::new();
        c.set_dnd(true);
        let now = t0();
        c.notify(req("x", "quiet"), now, 0);
        assert!(c.toasts().is_empty(), "DND hides normal toasts");
        assert_eq!(c.count(), 1, "but they still land in the list");
        let mut r = req("x", "alarm");
        r.urgency = Urgency::Critical;
        c.notify(r, now, 0);
        assert_eq!(c.toasts().len(), 1);
        let (_, next) = c.tick(now + Duration::from_secs(3600));
        assert_eq!(c.toasts().len(), 1, "critical never times out");
        assert_eq!(next, None);
    }

    #[test]
    fn turning_dnd_on_hides_current_toasts_but_not_critical() {
        let mut c = Center::new();
        let now = t0();
        c.notify(req("x", "a"), now, 0);
        let mut r = req("x", "b");
        r.urgency = Urgency::Critical;
        c.notify(r, now, 0);
        assert_eq!(c.toasts().len(), 2);
        c.set_dnd(true);
        assert_eq!(c.toasts().len(), 1);
        assert_eq!(c.toasts()[0].summary, "b");
    }

    #[test]
    fn opening_the_panel_hides_toasts_and_blocks_new_ones() {
        let mut c = Center::new();
        let now = t0();
        c.notify(req("x", "a"), now, 0);
        c.set_panel_open(true);
        assert!(c.toasts().is_empty());
        c.notify(req("x", "b"), now, 0);
        assert!(c.toasts().is_empty());
        assert_eq!(c.count(), 2);
        c.set_panel_open(false);
        assert!(c.toasts().is_empty(), "nothing pops up retroactively");
    }

    #[test]
    fn at_most_three_toasts_newest_first() {
        let mut c = Center::new();
        let now = t0();
        for i in 0..5 {
            c.notify(req("x", &format!("n{i}")), now, 0);
        }
        let t: Vec<_> = c.toasts().iter().map(|n| n.summary.clone()).collect();
        assert_eq!(t, vec!["n4", "n3", "n2"]);
    }

    #[test]
    fn close_dismiss_and_clear_report_the_right_reason() {
        let mut c = Center::new();
        let (a, _) = c.notify(req("x", "a"), t0(), 0);
        let (b, _) = c.notify(req("x", "b"), t0(), 0);
        let (d, _) = c.notify(req("x", "d"), t0(), 0);
        assert_eq!(c.close(a), vec![Out::Closed(a, REASON_CLOSED)]);
        assert_eq!(c.close(a), vec![], "unknown id is a no-op");
        assert_eq!(c.dismiss(b), vec![Out::Closed(b, REASON_DISMISSED)]);
        assert_eq!(c.clear_all(), vec![Out::Closed(d, REASON_DISMISSED)]);
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn activate_invokes_then_dismisses_unless_resident() {
        let mut c = Center::new();
        let mut r = req("x", "a");
        r.actions = vec![("default".into(), "Open".into()), ("reply".into(), "Reply".into())];
        let (a, _) = c.notify(r.clone(), t0(), 0);
        assert_eq!(c.activate(a, "nope"), vec![], "unknown key does nothing");
        assert_eq!(
            c.activate(a, "reply"),
            vec![Out::Action(a, "reply".into()), Out::Closed(a, REASON_DISMISSED)]
        );
        r.resident = true;
        let (b, _) = c.notify(r, t0(), 0);
        assert_eq!(c.activate(b, "default"), vec![Out::Action(b, "default".into())]);
        assert!(c.get(b).is_some(), "resident stays");
        assert!(c.get(b).unwrap().toast == Toast::Hidden);
    }

    #[test]
    fn hover_postpones_the_toast() {
        let mut c = Center::new();
        let now = t0();
        let (id, _) = c.notify(req("x", "a"), now, 0);
        c.postpone_toast(id, now + Duration::from_secs(5));
        let (_, next) = c.tick(now + Duration::from_secs(7));
        assert_eq!(next, Some(now + Duration::from_secs(11)));
        assert_eq!(c.toasts().len(), 1);
    }

    #[test]
    fn history_is_capped_and_overflow_is_announced() {
        let mut c = Center::new();
        let mut all = Vec::new();
        for i in 0..(MAX_ITEMS + 3) {
            let (_, outs) = c.notify(req("x", &format!("{i}")), t0(), 0);
            all.extend(outs);
        }
        assert_eq!(c.count(), MAX_ITEMS);
        assert_eq!(all, vec![Out::Closed(1, REASON_UNDEFINED), Out::Closed(2, REASON_UNDEFINED), Out::Closed(3, REASON_UNDEFINED)]);
    }

    fn hints(extra: Vec<(&str, Value)>) -> Value {
        Value::dict_sv(extra.into_iter().map(|(k, v)| (k.to_string(), v)))
    }

    fn call_body(actions: &[&str], h: Value, timeout: i32) -> Vec<Value> {
        vec![
            Value::s("app"),
            Value::U32(0),
            Value::s("dialog-information"),
            Value::s("Summary"),
            Value::s("Body <b>bold</b>"),
            Value::strings(actions.iter().copied()),
            h,
            Value::I32(timeout),
        ]
    }

    #[test]
    fn parses_a_notify_call() {
        let h = hints(vec![
            ("urgency", Value::Byte(2)),
            ("category", Value::s("im.received")),
            ("desktop-entry", Value::s("org.gnome.Fractal")),
            ("resident", Value::Bool(true)),
            ("value", Value::I32(150)),
            ("x-canonical-private-synchronous", Value::s("volume")),
            ("image-path", Value::s("/tmp/a.png")),
        ]);
        let r = parse_notify(&call_body(&["default", "Open", "reply", "Reply"], h, 3000)).unwrap();
        assert_eq!(r.app_name, "app");
        assert_eq!(r.summary, "Summary");
        assert_eq!(r.urgency, Urgency::Critical);
        assert_eq!(r.actions, vec![("default".to_string(), "Open".to_string()), ("reply".to_string(), "Reply".to_string())]);
        assert_eq!(r.category, "im.received");
        assert!(r.resident && !r.transient);
        assert_eq!(r.progress, Some(100), "clamped");
        assert_eq!(r.sync_tag.as_deref(), Some("volume"));
        assert_eq!(r.image_path.as_deref(), Some("/tmp/a.png"));
        assert_eq!(r.timeout_ms, 3000);
    }

    #[test]
    fn odd_calls_do_not_panic() {
        // wrong signature
        assert!(parse_notify(&[Value::s("x")]).is_err());
        // odd number of action strings: the dangling key is ignored
        let r = parse_notify(&call_body(&["a", "A", "b"], hints(vec![]), -1)).unwrap();
        assert_eq!(r.actions.len(), 1);
        // urgency as another integer type, unknown value => normal
        let r = parse_notify(&call_body(&[], hints(vec![("urgency", Value::U32(7))]), -1)).unwrap();
        assert_eq!(r.urgency, Urgency::Normal);
        let r = parse_notify(&call_body(&[], hints(vec![("urgency", Value::I32(0))]), -1)).unwrap();
        assert_eq!(r.urgency, Urgency::Low);
    }

    fn img_value(w: i32, h: i32, stride: i32, alpha: bool, bits: i32, ch: i32, data: Vec<u8>) -> Value {
        Value::Struct(vec![
            Value::I32(w),
            Value::I32(h),
            Value::I32(stride),
            Value::Bool(alpha),
            Value::I32(bits),
            Value::I32(ch),
            Value::Array("y".into(), data.into_iter().map(Value::Byte).collect()),
        ])
    }

    #[test]
    fn image_data_rgb_with_row_padding() {
        // 2x2 RGB, stride 8 (2 bytes padding per row)
        let data = vec![1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0];
        let r = parse_notify(&call_body(&[], hints(vec![("image-data", img_value(2, 2, 8, false, 8, 3, data))]), -1)).unwrap();
        let im = r.image.unwrap();
        assert_eq!((im.w, im.h), (2, 2));
        assert_eq!(im.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]);
    }

    #[test]
    fn image_data_rgba_and_legacy_names() {
        let data = vec![9, 8, 7, 6];
        let r = parse_notify(&call_body(&[], hints(vec![("icon_data", img_value(1, 1, 4, true, 8, 4, data))]), -1)).unwrap();
        assert_eq!(r.image.unwrap().rgba, vec![9, 8, 7, 6]);
    }

    #[test]
    fn bad_image_data_is_dropped_not_fatal() {
        let bad = [
            img_value(2, 2, 8, false, 8, 3, vec![0; 5]),     // too little data
            img_value(0, 2, 8, false, 8, 3, vec![]),         // zero width
            img_value(2, 2, 8, false, 16, 3, vec![0; 16]),   // 16 bit
            img_value(2, 2, 8, true, 8, 3, vec![0; 16]),     // alpha flag disagrees with channels
            img_value(100000, 100000, 4, true, 8, 4, vec![]), // absurd size
            img_value(1, 1, 2, false, 8, 3, vec![0; 3]),     // stride < row
            Value::s("not an image"),
        ];
        for b in bad {
            let r = parse_notify(&call_body(&[], hints(vec![("image-data", b)]), -1)).unwrap();
            assert!(r.image.is_none());
        }
    }

    #[test]
    fn markup_is_reduced_to_text() {
        assert_eq!(strip_markup("a <b>bold</b> &amp; <i>it</i>"), "a bold & it");
        assert_eq!(strip_markup("<a href=\"http://x\">link</a>"), "link");
        assert_eq!(strip_markup("1 < 2 and 3 > 2"), "1 < 2 and 3 > 2");
        assert_eq!(strip_markup("x &lt;3&gt; &#65;&#x42; &bogus; &"), "x <3> AB &bogus; &");
        assert_eq!(strip_markup("line<br/>two"), "line\ntwo");
        assert_eq!(strip_markup("日本語 <b>太字</b>"), "日本語 太字");
        assert_eq!(strip_markup("<"), "<");
        assert_eq!(strip_markup("&"), "&");
    }
}
