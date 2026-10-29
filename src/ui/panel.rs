//! Layout and painting of the notification center (media card, header,
//! notification list) and of the toasts. Everything is in logical pixels; the
//! `Painter` scales it. With no canvas the same code only measures.

use std::sync::Arc;

use super::paint::{theme::*, Painter, R};
use crate::gfx::canvas::Color;
use crate::gfx::icons::Icon;
use crate::gfx::text::Style;
use crate::model::fmt_time;
use crate::notify::{Notification, Urgency};

pub const CARD_W: f32 = 400.0;
/// Transparent border around the card: the gap to the bar and the screen edge.
/// Clicks there fall through to the backdrop (the input region is the card only).
pub const MARGIN: f32 = 18.0;
pub const TOAST_W: f32 = 380.0;
const PAD: f32 = 12.0;
const GAP: f32 = 8.0;
const ICON: f32 = 40.0;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HitId {
    Prev,
    PlayPause,
    Next,
    Seek,
    Volume,
    Mute,
    Dnd,
    Clear,
    /// the body of a notification (runs its default action)
    Item(u32),
    Close(u32),
    Action(u32, String),
    /// the scrolling list area
    List,
}

impl HitId {
    /// Does the pointer turn into a hand over this?
    pub fn clickable(&self) -> bool {
        !matches!(self, HitId::List)
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub r: R,
    pub id: HitId,
}

/// Cover art already resized to `dev` x `dev` device pixels (premultiplied RGBA).
#[derive(Clone)]
pub struct Art {
    pub dev: usize,
    pub px: Arc<Vec<u8>>,
}

#[derive(Clone, Default)]
pub struct MediaView {
    pub identity: String,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_us: i64,
    pub length_us: i64,
    pub volume: f64,
    pub can_seek: bool,
    pub art: Option<Art>,
}

pub type Images<'a> = &'a dyn Fn(&Notification, usize) -> Option<Arc<Vec<u8>>>;

pub struct PanelState<'a> {
    pub media: Option<&'a MediaView>,
    /// newest first
    pub items: &'a [&'a Notification],
    pub dnd: bool,
    pub now_unix: i64,
    pub hover: Option<&'a HitId>,
    pub pressed: Option<&'a HitId>,
    /// a slider being dragged and where it currently is (0..1)
    pub drag: Option<(HitId, f64)>,
    pub scroll: f32,
    /// the card (without its margin) never gets taller than this
    pub max_card_h: f32,
    pub images: Images<'a>,
}

/// What was laid out; the event loop uses it for hit testing.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// surface size in logical pixels
    pub w: f32,
    pub h: f32,
    /// the visible card (the only part that takes pointer input)
    pub card: Option<R>,
    pub hits: Vec<Hit>,
    pub list_view: Option<R>,
    pub list_h: f32,
    pub scroll: f32,
    pub seek: Option<R>,
    pub volume: Option<R>,
}

impl Frame {
    /// Topmost thing under (x, y).
    pub fn hit(&self, x: f32, y: f32) -> Option<&Hit> {
        self.hits.iter().rev().find(|h| h.r.contains(x, y))
    }
}

pub fn surface_w() -> f32 {
    CARD_W + 2.0 * MARGIN
}

fn is_hover(st: Option<&HitId>, id: &HitId) -> bool {
    st == Some(id)
}

/// "now", "5m", "3h", "2d"
pub fn ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..=44 => "now".into(),
        45..=3599 => format!("{}m", (d + 30) / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

// ------------------------------------------------------------------ pieces

struct Ctx<'a> {
    now: i64,
    hover: Option<&'a HitId>,
    pressed: Option<&'a HitId>,
    images: Images<'a>,
}

fn press_shade(ctx: &Ctx, id: &HitId, base: Color, hover: Color, down: Color) -> Color {
    if ctx.pressed == Some(id) && ctx.hover == Some(id) {
        down
    } else if is_hover(ctx.hover, id) {
        hover
    } else {
        base
    }
}

fn slider(p: &mut Painter, track: R, frac: f64, active: bool, enabled: bool) {
    let f = frac.clamp(0.0, 1.0) as f32;
    let h = if active { 6.0 } else { 4.0 };
    let t = R::new(track.x, track.cy() - h / 2.0, track.w, h);
    p.rrect(t, h / 2.0, TRACK);
    if f > 0.0 {
        p.rrect(R::new(t.x, t.y, (t.w * f).max(h), h), h / 2.0, if enabled { FILL } else { TRACK });
    }
    if enabled {
        let d = if active { 14.0 } else { 12.0 };
        let kx = (t.x + t.w * f).clamp(t.x + d / 2.0, t.right() - d / 2.0);
        p.rrect(R::new(kx - d / 2.0, track.cy() - d / 2.0, d, d), d / 2.0, KNOB);
    }
}

fn volume_icon(v: f64) -> Icon {
    if v <= 0.001 {
        Icon::Mute
    } else if v < 0.5 {
        Icon::VolumeLow
    } else {
        Icon::Volume
    }
}

/// The media card. `r` is its rectangle; returns nothing, hits go into `f`.
fn draw_media(p: &mut Painter, ctx: &Ctx, r: R, m: &MediaView, drag: &Option<(HitId, f64)>, f: &mut Frame) {
    p.rrect(r, 14.0, CARD);
    p.stroke(r, 14.0, 1.0, BORDER);

    // art
    let art = R::new(r.x + 16.0, r.y + 16.0, 76.0, 76.0);
    match &m.art {
        Some(a) if a.dev == p.dev(76.0) => p.image(art, &a.px, a.dev, 10.0, 1.0),
        _ => {
            p.rrect(art, 10.0, ART_BG);
            p.icon(Icon::Note, art.cx(), art.cy(), 30.0, TEXT3);
        }
    }

    // text
    let tx = art.right() + 14.0;
    let tw = r.right() - 16.0 - tx;
    let (title, artist) = if m.title.is_empty() && m.artist.is_empty() {
        (m.identity.clone(), "Nothing playing".to_string())
    } else {
        (m.title.clone(), m.artist.clone())
    };
    let title = p.ellipsize(Style::Bold, &title, 15.0, tw);
    p.text(Style::Bold, tx, r.y + 18.0, &title, 15.0, TEXT);
    let artist = p.ellipsize(Style::Regular, &artist, 13.5, tw);
    p.text(Style::Regular, tx, r.y + 42.0, &artist, 13.5, TEXT2);
    if !m.album.is_empty() {
        let album = p.ellipsize(Style::Regular, &m.album, 12.5, tw);
        p.text(Style::Regular, tx, r.y + 63.0, &album, 12.5, TEXT3);
    }

    // seek row
    let seek_cy = r.y + 112.0;
    let dragging_seek = matches!(drag, Some((HitId::Seek, _)));
    let (pos, frac) = match drag {
        Some((HitId::Seek, f)) => ((f * m.length_us as f64) as i64, *f),
        _ => (m.position_us, if m.length_us > 0 { m.position_us as f64 / m.length_us as f64 } else { 0.0 }),
    };
    let left = fmt_time(pos);
    let right = if m.length_us > 0 { fmt_time(m.length_us) } else { "--:--".to_string() };
    p.text_mid(Style::Regular, r.x + 16.0, seek_cy, &left, 12.0, TEXT3);
    p.text_mid_right(Style::Regular, r.right() - 16.0, seek_cy, &right, 12.0, TEXT3);
    let track = R::new(r.x + 16.0 + 40.0, seek_cy - 8.0, r.w - 32.0 - 80.0, 16.0);
    let seekable = m.can_seek && m.length_us > 0;
    slider(p, track, frac, dragging_seek || (seekable && is_hover(ctx.hover, &HitId::Seek)), seekable);
    f.seek = Some(track);
    if seekable {
        f.hits.push(Hit { r: R::new(track.x - 6.0, track.y - 6.0, track.w + 12.0, track.h + 12.0), id: HitId::Seek });
    }

    // transport
    let cy = r.y + 150.0;
    let cx = r.cx();
    let who = p.ellipsize(Style::Regular, &m.identity, 12.0, 96.0);
    p.text_mid(Style::Regular, r.x + 16.0, cy, &who, 12.0, TEXT3);
    for (id, icon, dx, d, filled) in [
        (HitId::Prev, Icon::Prev, -54.0, 36.0, false),
        (HitId::PlayPause, if m.playing { Icon::Pause } else { Icon::Play }, 0.0, 44.0, true),
        (HitId::Next, Icon::Next, 54.0, 36.0, false),
    ] {
        let b = R::new(cx + dx - d / 2.0, cy - d / 2.0, d, d);
        let hovered = is_hover(ctx.hover, &id);
        if filled || hovered {
            let base = if filled { CONTROL } else { Color::rgba(0, 0, 0, 0) };
            let shade = if filled {
                press_shade(ctx, &id, CONTROL, CONTROL_HOVER, CONTROL_PRESSED)
            } else {
                press_shade(ctx, &id, base, CONTROL_HOVER, CONTROL_PRESSED)
            };
            p.rrect(b, d / 2.0, shade);
        }
        p.icon(icon, b.cx(), b.cy(), if filled { 22.0 } else { 18.0 }, TEXT);
        f.hits.push(Hit { r: b, id });
    }

    // volume
    let vol_frac = match drag {
        Some((HitId::Volume, v)) => *v,
        _ => m.volume,
    };
    let vi = R::new(r.right() - 16.0 - 74.0 - 8.0 - 24.0, cy - 12.0, 24.0, 24.0);
    p.icon(volume_icon(vol_frac), vi.cx(), vi.cy(), 16.0, if is_hover(ctx.hover, &HitId::Mute) { TEXT } else { TEXT2 });
    f.hits.push(Hit { r: vi, id: HitId::Mute });
    let vt = R::new(vi.right() + 8.0, cy - 8.0, 74.0, 16.0);
    let dragging_vol = matches!(drag, Some((HitId::Volume, _)));
    slider(p, vt, vol_frac, dragging_vol || is_hover(ctx.hover, &HitId::Volume), true);
    f.volume = Some(vt);
    f.hits.push(Hit { r: R::new(vt.x - 6.0, vt.y - 6.0, vt.w + 12.0, vt.h + 12.0), id: HitId::Volume });
}

const MEDIA_H: f32 = 180.0;

fn pill(p: &mut Painter, ctx: &Ctx, id: HitId, right: f32, cy: f32, icon: Icon, label: &str, active: bool, f: &mut Frame) -> f32 {
    let tw = p.width(Style::Regular, label, 12.5);
    let w = 12.0 + 14.0 + 6.0 + tw + 14.0;
    let r = R::new(right - w, cy - 15.0, w, 30.0);
    let (bg, fg) = if active {
        (Color::rgb(0xdc, 0xdc, 0xe0), Color::rgb(0x1f, 0x1f, 0x22))
    } else {
        (press_shade(ctx, &id, CONTROL, CONTROL_HOVER, CONTROL_PRESSED), TEXT)
    };
    p.rrect(r, 15.0, bg);
    p.icon(icon, r.x + 12.0 + 7.0, cy, 14.0, fg);
    p.text_mid(Style::Regular, r.x + 12.0 + 14.0 + 6.0, cy, label, 12.5, fg);
    f.hits.push(Hit { r, id });
    w
}

fn avatar_letter(app: &str) -> String {
    app.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

/// One notification card at (x, y) with width `w`. Returns its height.
/// `clip` limits the hit rectangles to what is actually visible.
fn draw_item(p: &mut Painter, ctx: &Ctx, x: f32, y: f32, w: f32, n: &Notification, show_close: bool, clip: Option<R>, f: &mut Frame) -> f32 {
    let tx = x + PAD + ICON + 10.0;
    let tw = x + w - PAD - tx;
    let item_id = HitId::Item(n.id);
    let close_id = HitId::Close(n.id);
    let hovered = is_hover(ctx.hover, &item_id) || is_hover(ctx.hover, &close_id) || matches!(ctx.hover, Some(HitId::Action(i, _)) if *i == n.id);

    // measure first: the card background needs the final height
    let body_lines = if n.body.is_empty() { Vec::new() } else { p.wrap(Style::Regular, &n.body, 13.0, tw, 4) };
    let body_lh = p.line_h(Style::Regular, 13.0) + 1.5;
    let name_lh = p.line_h(Style::Regular, 11.5);
    let sum_lh = p.line_h(Style::Bold, 14.0);
    let mut cy = y + PAD;
    let name_y = cy;
    cy += name_lh + 2.0;
    let sum_y = cy;
    cy += sum_lh + 4.0;
    let body_y = cy;
    cy += body_lh * body_lines.len() as f32;
    let progress_y = if n.progress.is_some() {
        cy += 6.0;
        let v = cy;
        cy += 4.0;
        Some(v)
    } else {
        None
    };
    let actions: Vec<&(String, String)> = n.button_actions().take(3).collect();
    let actions_y = if !actions.is_empty() {
        cy += 8.0;
        let v = cy;
        cy += 28.0;
        Some(v)
    } else {
        None
    };
    let h = (cy - y).max(ICON) + PAD;
    let h = h.max(PAD * 2.0 + ICON);

    let card = R::new(x, y, w, h);
    let clickable = n.has_default_action();
    let bg = if hovered && clickable { CARD_HOVER } else { CARD };
    p.rrect(card, 12.0, bg);
    p.stroke(card, 12.0, 1.0, BORDER);
    if n.urgency == Urgency::Critical {
        p.rrect(R::new(x + 1.0, y + 12.0, 3.0, h - 24.0), 1.5, CRITICAL);
    }

    // hits (the body first, so buttons on top win)
    let visible = |r: R| -> Option<R> {
        match clip {
            Some(c) => r.intersect(&c),
            None => Some(r),
        }
    };
    if let Some(r) = visible(card) {
        f.hits.push(Hit { r, id: item_id });
    }

    // icon / image / avatar
    let ir = R::new(x + PAD, y + PAD, ICON, ICON);
    match (ctx.images)(n, p.dev(ICON)) {
        Some(px) => p.image(ir, &px, p.dev(ICON), 9.0, 1.0),
        None => {
            p.rrect(ir, ICON / 2.0, CONTROL);
            p.text_center(Style::Bold, ir.cx(), ir.cy(), &avatar_letter(&n.app_name), 17.0, TEXT2);
        }
    }

    // app name and age
    let mut head = if n.app_name.is_empty() { "Notification".to_string() } else { n.app_name.clone() };
    head.push_str("  ·  ");
    head.push_str(&ago(ctx.now, n.created_unix));
    let head = p.ellipsize(Style::Regular, &head, 11.5, tw - 28.0);
    p.text(Style::Regular, tx, name_y, &head, 11.5, TEXT3);

    if show_close || hovered {
        let c = R::new(x + w - PAD - 22.0 + 4.0, y + 6.0, 22.0, 22.0);
        if is_hover(ctx.hover, &close_id) {
            p.rrect(c, 11.0, press_shade(ctx, &close_id, CONTROL, CONTROL_HOVER, CONTROL_PRESSED));
        }
        p.icon(Icon::Close, c.cx(), c.cy(), 12.0, if is_hover(ctx.hover, &close_id) { TEXT } else { TEXT3 });
        if let Some(r) = visible(c) {
            f.hits.push(Hit { r, id: close_id });
        }
    }

    // summary and body
    let summary = if n.summary.is_empty() { "(no title)" } else { n.summary.as_str() };
    let summary = p.ellipsize(Style::Bold, summary, 14.0, tw);
    p.text(Style::Bold, tx, sum_y, &summary, 14.0, TEXT);
    for (i, line) in body_lines.iter().enumerate() {
        p.text(Style::Regular, tx, body_y + body_lh * i as f32, line, 13.0, TEXT2);
    }

    // progress
    if let (Some(py), Some(v)) = (progress_y, n.progress) {
        let t = R::new(tx, py, tw, 4.0);
        p.rrect(t, 2.0, TRACK);
        let f = (v.clamp(0, 100) as f32) / 100.0;
        if f > 0.0 {
            p.rrect(R::new(t.x, t.y, (t.w * f).max(4.0), 4.0), 2.0, FILL);
        }
    }

    // action buttons
    if let Some(ay) = actions_y {
        let mut bx = tx;
        for (key, label) in actions {
            let label = p.ellipsize(Style::Regular, label, 12.5, 130.0);
            let bw = p.width(Style::Regular, &label, 12.5) + 24.0;
            if bx + bw > x + w - PAD + 0.5 {
                break;
            }
            let b = R::new(bx, ay, bw, 28.0);
            let id = HitId::Action(n.id, key.clone());
            p.rrect(b, 14.0, press_shade(ctx, &id, CONTROL, CONTROL_HOVER, CONTROL_PRESSED));
            p.text_center(Style::Regular, b.cx(), b.cy(), &label, 12.5, TEXT);
            if let Some(r) = visible(b) {
                f.hits.push(Hit { r, id });
            }
            bx += bw + 8.0;
        }
    }
    h
}

fn empty_state(p: &mut Painter, r: R) {
    p.icon(Icon::Bell, r.cx(), r.y + 38.0, 28.0, TEXT3);
    p.text_center(Style::Regular, r.cx(), r.y + 72.0, "No Notifications", 14.0, TEXT3);
}

// ------------------------------------------------------------------ the panel

/// Lay out and (with a canvas) paint the whole panel.
pub fn draw_panel(p: &mut Painter, st: &PanelState) -> Frame {
    let ctx = Ctx { now: st.now_unix, hover: st.hover, pressed: st.pressed, images: st.images };
    let mut f = Frame { w: surface_w(), ..Default::default() };

    // pass 1: heights
    let media_h = if st.media.is_some() { MEDIA_H + GAP + 4.0 } else { 0.0 };
    let header_h = 44.0;
    let mut heights = Vec::with_capacity(st.items.len());
    {
        let mut scratch = Frame::default();
        for n in st.items {
            let h = draw_item_measure(p, &ctx, n, &mut scratch);
            heights.push(h);
        }
    }
    let list_h = if st.items.is_empty() { 104.0 } else { heights.iter().sum::<f32>() + GAP * (heights.len() as f32 - 1.0) };
    let chrome = PAD + media_h + header_h + PAD;
    let avail = (st.max_card_h - chrome).max(120.0);
    let view_h = list_h.min(avail);
    let card_h = chrome + view_h;
    f.h = card_h + 2.0 * MARGIN;
    f.list_h = list_h;
    let card = R::new(MARGIN, MARGIN, CARD_W, card_h);
    f.card = Some(card);
    let max_scroll = (list_h - view_h).max(0.0);
    let scroll = st.scroll.clamp(0.0, max_scroll);
    f.scroll = scroll;

    // card
    p.rrect(card, 18.0, PANEL_BG);
    p.stroke(card, 18.0, 1.0, BORDER);

    let mut y = card.y + PAD;
    if let Some(m) = st.media {
        draw_media(p, &ctx, R::new(card.x + PAD, y, CARD_W - 2.0 * PAD, MEDIA_H), m, &st.drag, &mut f);
        y += MEDIA_H + GAP + 4.0;
    }

    // header
    let hcy = y + header_h / 2.0 - 2.0;
    p.text_mid(Style::Bold, card.x + PAD + 4.0, hcy, "Notifications", 15.0, TEXT);
    let mut right = card.right() - PAD;
    let dnd_w = pill(p, &ctx, HitId::Dnd, right, hcy, Icon::BellOff, "Do Not Disturb", st.dnd, &mut f);
    right -= dnd_w + 8.0;
    if !st.items.is_empty() {
        pill(p, &ctx, HitId::Clear, right, hcy, Icon::Trash, "Clear", false, &mut f);
    }
    y += header_h;

    // list
    let view = R::new(card.x + PAD, y, CARD_W - 2.0 * PAD, view_h);
    f.list_view = Some(view);
    f.hits.push(Hit { r: view, id: HitId::List });
    if st.items.is_empty() {
        empty_state(p, view);
    } else {
        p.clip(Some(view));
        let mut iy = view.y - scroll;
        for (n, &h) in st.items.iter().zip(&heights) {
            if iy + h >= view.y && iy <= view.bottom() {
                draw_item(p, &ctx, view.x, iy, view.w, n, false, Some(view), &mut f);
            }
            iy += h + GAP;
        }
        p.clip(None);
        if max_scroll > 0.0 {
            let bar_h = (view_h * view_h / list_h).max(24.0);
            let by = view.y + (view_h - bar_h) * (scroll / max_scroll);
            p.rrect(R::new(view.right() - 4.0, by, 3.0, bar_h), 1.5, Color::rgba(255, 255, 255, 60));
        }
    }
    f
}

/// Height of an item without painting it.
fn draw_item_measure(p: &mut Painter, ctx: &Ctx, n: &Notification, scratch: &mut Frame) -> f32 {
    // run the real layout with painting switched off
    let cv = p.cv.take();
    let h = draw_item(p, ctx, 0.0, 0.0, CARD_W - 2.0 * PAD, n, false, None, scratch);
    p.cv = cv;
    h
}

// ------------------------------------------------------------------ toasts

/// Stack of toast cards, newest on top.
pub fn draw_toasts(p: &mut Painter, items: &[&Notification], now_unix: i64, hover: Option<&HitId>, pressed: Option<&HitId>, images: Images) -> Frame {
    let ctx = Ctx { now: now_unix, hover, pressed, images };
    let mut f = Frame { w: TOAST_W + 2.0 * MARGIN, ..Default::default() };
    let mut y = MARGIN;
    let mut scratch = Frame::default();
    for n in items {
        let cv = p.cv.take();
        let h = draw_item(p, &ctx, 0.0, 0.0, TOAST_W, n, true, None, &mut scratch);
        p.cv = cv;
        let hh = draw_item(p, &ctx, MARGIN, y, TOAST_W, n, true, None, &mut f);
        debug_assert!((hh - h).abs() < 0.01);
        y += h + 10.0;
    }
    if !items.is_empty() {
        y -= 10.0;
        f.card = Some(R::new(MARGIN, MARGIN, TOAST_W, y - MARGIN));
    }
    f.h = y + MARGIN;
    f
}
