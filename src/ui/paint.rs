//! Drawing in logical pixels. All layout code works in logical units and a
//! `Painter` scales them to the device pixels of the buffer. With no canvas it
//! only measures, so the same code answers "how tall will this be?".

use crate::gfx::canvas::{Canvas, Color, Rect};
use crate::gfx::icons::{self, Icon};
use crate::gfx::text::{Fonts, Style};
use crate::model::Theme;

pub struct Painter<'a> {
    pub cv: Option<&'a mut Canvas>,
    /// device pixels per logical pixel
    pub s: f32,
    pub fonts: &'a mut Fonts,
    pub theme: Theme,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> R {
        R { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn inset(&self, d: f32) -> R {
        R::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    pub fn intersect(&self, o: &R) -> Option<R> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        (x1 > x0 && y1 > y0).then(|| R::new(x0, y0, x1 - x0, y1 - y0))
    }
    pub fn translate(&self, dx: f32, dy: f32) -> R {
        R::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

impl<'a> Painter<'a> {
    pub fn new(cv: Option<&'a mut Canvas>, s: f32, fonts: &'a mut Fonts) -> Painter<'a> {
        Painter { cv, s, fonts, theme: Theme::Default }
    }

    pub fn themed(mut self, theme: Theme) -> Painter<'a> {
        self.theme = theme;
        self
    }

    pub fn retro(&self) -> bool {
        self.theme == Theme::Retro
    }

    /// The colours of the current theme.
    pub fn pal(&self) -> &'static Pal {
        match self.theme {
            Theme::Default => &DEFAULT,
            Theme::Retro => &RETRO,
        }
    }

    /// Corner radius to use: the retro theme has none.
    pub fn radius(&self, r: f32) -> f32 {
        if self.retro() {
            0.0
        } else {
            r
        }
    }

    pub fn drawing(&self) -> bool {
        self.cv.is_some()
    }

    /// Filled rounded rectangle. The retro theme draws a plain pixel-aligned one.
    pub fn rrect(&mut self, r: R, radius: f32, c: Color) {
        let s = self.s;
        if self.retro() {
            let b = self.snap(r);
            if let Some(cv) = self.cv.as_deref_mut() {
                cv.fill_rect(b, c);
            }
        } else if let Some(cv) = self.cv.as_deref_mut() {
            cv.fill_rrect(r.x * s, r.y * s, r.w * s, r.h * s, radius * s, c);
        }
    }

    pub fn stroke(&mut self, r: R, radius: f32, width: f32, c: Color) {
        let s = self.s;
        if self.retro() {
            let b = self.snap(r);
            let bw = ((width * s).round() as i32).max(1);
            self.frame_px(b, bw, c, c);
        } else if let Some(cv) = self.cv.as_deref_mut() {
            cv.stroke_rrect(r.x * s, r.y * s, r.w * s, r.h * s, radius * s, width * s, c);
        }
    }

    // ---- themed surfaces: the default theme paints rounded flat shapes, the
    // retro one square beveled ones. Layout never depends on which is active.

    /// The main window.
    pub fn panel(&mut self, r: R, radius: f32) {
        let pal = self.pal();
        if self.retro() {
            let b = self.snap(r);
            self.fill_px(b, pal.panel_bg);
            self.bevel_px(b, true);
        } else {
            self.rrect(r, radius, pal.panel_bg);
            self.stroke(r, radius, 1.0, pal.border);
        }
    }

    /// A raised block on the panel (media card, notification).
    pub fn card(&mut self, r: R, radius: f32, bg: Color) {
        let pal = self.pal();
        if self.retro() {
            let b = self.snap(r);
            self.fill_px(b, bg);
            self.bevel_px(b, true);
        } else {
            self.rrect(r, radius, bg);
            self.stroke(r, radius, 1.0, pal.border);
        }
    }

    /// A push button; `down` is the pressed (or toggled on) look.
    pub fn button(&mut self, r: R, radius: f32, bg: Color, down: bool) {
        if self.retro() {
            let b = self.snap(r);
            self.fill_px(b, bg);
            self.bevel_px(b, !down);
        } else {
            self.rrect(r, radius, bg);
        }
    }

    /// A recessed area (slider groove, progress track, empty cover). Returns the
    /// rectangle inside its edge, where fills should go.
    pub fn well(&mut self, r: R, radius: f32, bg: Color) -> R {
        if self.retro() {
            let b = self.snap(r);
            self.fill_px(b, bg);
            self.groove_px(b);
            r.inset(self.bevel_w() as f32 / self.s)
        } else {
            self.rrect(r, radius, bg);
            r
        }
    }

    /// A thin sunken frame over the edge of a picture (retro only).
    pub fn image_frame(&mut self, r: R) {
        if self.retro() {
            let b = self.snap(r);
            self.groove_px(b);
        }
    }

    // ---- pixel-aligned helpers (device pixels)

    fn bevel_w(&self) -> i32 {
        (self.s.round() as i32).max(1)
    }

    /// `r` rounded to whole device pixels, never empty.
    fn snap(&self, r: R) -> Rect {
        let s = self.s;
        let (x0, y0) = ((r.x * s).round() as i32, (r.y * s).round() as i32);
        let (x1, y1) = ((r.right() * s).round() as i32, (r.bottom() * s).round() as i32);
        Rect { x0, y0, x1: x1.max(x0 + 1), y1: y1.max(y0 + 1) }
    }

    fn fill_px(&mut self, b: Rect, c: Color) {
        if let Some(cv) = self.cv.as_deref_mut() {
            cv.fill_rect(b, c);
        }
    }

    /// A `bw`-wide frame inside `b`: `tl` on the top and left, `br` on the bottom and right.
    fn frame_px(&mut self, b: Rect, bw: i32, tl: Color, br: Color) {
        let Some(cv) = self.cv.as_deref_mut() else { return };
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        let bw = bw.min(w / 2).min(h / 2).max(0);
        if bw == 0 {
            return;
        }
        cv.fill_rect(Rect::new(b.x0, b.y0, w, bw), tl);
        cv.fill_rect(Rect::new(b.x0, b.y0, bw, h), tl);
        cv.fill_rect(Rect::new(b.x0, b.y1 - bw, w, bw), br);
        cv.fill_rect(Rect::new(b.x1 - bw, b.y0, bw, h), br);
    }

    /// Window Maker style box edge: a black outline, then a bevel inside it,
    /// light on the top/left and dark on the bottom/right when raised and the
    /// other way round when sunken. One device-pixel step per line (two at 2x).
    fn bevel_px(&mut self, b: Rect, raised: bool) {
        let pal = self.pal();
        let bw = self.bevel_w();
        self.frame_px(b, bw, pal.outline, pal.outline);
        let inner = Rect { x0: b.x0 + bw, y0: b.y0 + bw, x1: b.x1 - bw, y1: b.y1 - bw };
        let (tl, br) = if raised { (pal.hi, pal.lo) } else { (pal.lo, pal.hi) };
        self.frame_px(inner, bw, tl, br);
    }

    /// A thin sunken edge without the outline, for small recesses such as tracks.
    fn groove_px(&mut self, b: Rect) {
        let pal = self.pal();
        self.frame_px(b, self.bevel_w(), pal.lo, pal.hi);
    }

    pub fn icon(&mut self, icon: Icon, cx: f32, cy: f32, size: f32, c: Color) {
        let s = self.s;
        if let Some(cv) = self.cv.as_deref_mut() {
            icons::draw(cv, icon, cx * s, cy * s, size * s, c);
        }
    }

    /// Restrict drawing to `r` (logical); None lifts the restriction.
    pub fn clip(&mut self, r: Option<R>) {
        let s = self.s;
        if let Some(cv) = self.cv.as_deref_mut() {
            cv.set_clip(r.map(|r| {
                let (x0, y0) = ((r.x * s).floor() as i32, (r.y * s).floor() as i32);
                let (x1, y1) = ((r.right() * s).ceil() as i32, (r.bottom() * s).ceil() as i32);
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }));
        }
    }

    /// Premultiplied RGBA pixels (already `dev` x `dev` device pixels) in a square at `r`.
    pub fn image(&mut self, r: R, px: &[u8], dev: usize, radius: f32, opacity: f32) {
        let s = self.s;
        if let Some(cv) = self.cv.as_deref_mut() {
            cv.draw_image((r.x * s).round() as i32, (r.y * s).round() as i32, dev, dev, px, radius * s, opacity);
        }
    }

    /// Device pixels of a logical length (what an image for a square of `l` must be).
    pub fn dev(&self, l: f32) -> usize {
        (l * self.s).round().max(1.0) as usize
    }

    // ---- text (sizes and positions in logical pixels)

    pub fn width(&mut self, style: Style, text: &str, px: f32) -> f32 {
        let s = self.s;
        self.fonts.measure(style, text, px * s) / s
    }

    /// Height of one line of text.
    pub fn line_h(&mut self, style: Style, px: f32) -> f32 {
        let s = self.s;
        // metrics: (ascent, descent as a positive number, line height)
        self.fonts.metrics(style, px * s).2 / s
    }

    pub fn ascent(&mut self, style: Style, px: f32) -> f32 {
        let s = self.s;
        self.fonts.metrics(style, px * s).0 / s
    }

    /// Text whose line box starts at `y`; returns the width drawn.
    pub fn text(&mut self, style: Style, x: f32, y: f32, text: &str, px: f32, c: Color) -> f32 {
        let s = self.s;
        let w = self.width(style, text, px);
        let base = y + self.ascent(style, px);
        if let Some(cv) = self.cv.as_deref_mut() {
            self.fonts.draw(cv, style, x * s, base * s, text, px * s, c);
        }
        w
    }

    /// Text vertically centred on `cy`.
    pub fn text_mid(&mut self, style: Style, x: f32, cy: f32, text: &str, px: f32, c: Color) -> f32 {
        let s = self.s;
        let (a, d, _) = self.fonts.metrics(style, px * s);
        // centre the cap height on `cy`
        let base = cy + (a - d) / 2.0 / s;
        let w = self.width(style, text, px);
        if let Some(cv) = self.cv.as_deref_mut() {
            self.fonts.draw(cv, style, x * s, base * s, text, px * s, c);
        }
        w
    }

    /// Right-aligned at `right`, centred on `cy`.
    pub fn text_mid_right(&mut self, style: Style, right: f32, cy: f32, text: &str, px: f32, c: Color) -> f32 {
        let w = self.width(style, text, px);
        self.text_mid(style, right - w, cy, text, px, c);
        w
    }

    /// Centred horizontally on `cx`, vertically on `cy`.
    pub fn text_center(&mut self, style: Style, cx: f32, cy: f32, text: &str, px: f32, c: Color) {
        let w = self.width(style, text, px);
        self.text_mid(style, cx - w / 2.0, cy, text, px, c);
    }

    pub fn ellipsize(&mut self, style: Style, text: &str, px: f32, max_w: f32) -> String {
        let s = self.s;
        self.fonts.ellipsize(style, text, px * s, max_w * s)
    }

    pub fn wrap(&mut self, style: Style, text: &str, px: f32, max_w: f32, max_lines: usize) -> Vec<String> {
        let s = self.s;
        self.fonts.wrap(style, text, px * s, max_w * s, max_lines)
    }
}

/// Every colour a theme decides. Layout and geometry are shared by both.
pub struct Pal {
    pub panel_bg: Color,
    pub card: Color,
    pub card_hover: Color,
    pub control: Color,
    pub control_hover: Color,
    pub control_pressed: Color,
    pub track: Color,
    pub fill: Color,
    pub knob: Color,
    pub text: Color,
    pub text2: Color,
    pub text3: Color,
    pub art_bg: Color,
    pub critical: Color,
    pub border: Color,
    /// a toggled-on pill (Do Not Disturb)
    pub active_bg: Color,
    pub active_fg: Color,
    /// the list's scroll bar
    pub scrollbar: Color,
    /// bevel colours (retro): light edge, dark edge, and the black outline
    pub hi: Color,
    pub lo: Color,
    pub outline: Color,
}

/// Dark greys, one neutral accent, red only for critical.
pub static DEFAULT: Pal = Pal {
    panel_bg: Color::rgb(0x1f, 0x1f, 0x22),
    card: Color::rgb(0x2b, 0x2b, 0x30),
    card_hover: Color::rgb(0x33, 0x33, 0x39),
    control: Color::rgb(0x3a, 0x3a, 0x40),
    control_hover: Color::rgb(0x46, 0x46, 0x4d),
    control_pressed: Color::rgb(0x2f, 0x2f, 0x34),
    track: Color::rgb(0x45, 0x45, 0x4b),
    fill: Color::rgb(0x9a, 0x9a, 0xa2),
    knob: Color::rgb(0xe4, 0xe4, 0xe8),
    text: Color::rgb(0xf2, 0xf2, 0xf4),
    text2: Color::rgb(0xb8, 0xb8, 0xc0),
    text3: Color::rgb(0x84, 0x84, 0x8d),
    art_bg: Color::rgb(0x3d, 0x3d, 0x44),
    critical: Color::rgb(0xe5, 0x48, 0x4d),
    border: Color::rgba(255, 255, 255, 22),
    active_bg: Color::rgb(0xdc, 0xdc, 0xe0),
    active_fg: Color::rgb(0x1f, 0x1f, 0x22),
    scrollbar: Color::rgba(255, 255, 255, 60),
    // not used by this theme
    hi: Color::rgb(0, 0, 0),
    lo: Color::rgb(0, 0, 0),
    outline: Color::rgb(0, 0, 0),
};

/// Flat gray, black text, black outlines with one-pixel bevels: Window Maker /
/// NeXTSTEP in spirit, in the exact colours of a matching retro Quickshell bar
/// (face #aeaaae, highlight #ffffff, shadow #555555, title blue #505075, soft grey sliders).
pub static RETRO: Pal = Pal {
    panel_bg: Color::rgb(0xae, 0xaa, 0xae),
    card: Color::rgb(0xae, 0xaa, 0xae),
    card_hover: Color::rgb(0xbe, 0xba, 0xbe),
    control: Color::rgb(0xae, 0xaa, 0xae),
    control_hover: Color::rgb(0xbe, 0xba, 0xbe),
    control_pressed: Color::rgb(0x9e, 0x9a, 0x9e),
    // sliders and progress bars: a soft grey trough with a darker grey fill
    track: Color::rgb(0x8e, 0x8a, 0x8e),
    fill: Color::rgb(0x5a, 0x56, 0x5a),
    knob: Color::rgb(0xae, 0xaa, 0xae),
    text: Color::rgb(0x00, 0x00, 0x00),
    text2: Color::rgb(0x1a, 0x1a, 0x1a),
    text3: Color::rgb(0x3a, 0x3a, 0x3a),
    art_bg: Color::rgb(0x8a, 0x86, 0x8a),
    critical: Color::rgb(0xb0, 0x00, 0x00),
    border: Color::rgba(0, 0, 0, 0),
    // a toggled-on button wears the title-bar blue
    active_bg: Color::rgb(0x50, 0x50, 0x75),
    active_fg: Color::rgb(0xff, 0xff, 0xff),
    scrollbar: Color::rgb(0xae, 0xaa, 0xae),
    hi: Color::rgb(0xff, 0xff, 0xff),
    lo: Color::rgb(0x55, 0x55, 0x55),
    outline: Color::rgb(0x00, 0x00, 0x00),
};
