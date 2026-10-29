//! Drawing in logical pixels. All layout code works in logical units and a
//! `Painter` scales them to the device pixels of the buffer. With no canvas it
//! only measures, so the same code answers "how tall will this be?".

use crate::gfx::canvas::{Canvas, Color, Rect};
use crate::gfx::icons::{self, Icon};
use crate::gfx::text::{Fonts, Style};

pub struct Painter<'a> {
    pub cv: Option<&'a mut Canvas>,
    /// device pixels per logical pixel
    pub s: f32,
    pub fonts: &'a mut Fonts,
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
        Painter { cv, s, fonts }
    }

    pub fn drawing(&self) -> bool {
        self.cv.is_some()
    }

    pub fn rrect(&mut self, r: R, radius: f32, c: Color) {
        let s = self.s;
        if let Some(cv) = self.cv.as_deref_mut() {
            cv.fill_rrect(r.x * s, r.y * s, r.w * s, r.h * s, radius * s, c);
        }
    }

    pub fn stroke(&mut self, r: R, radius: f32, width: f32, c: Color) {
        let s = self.s;
        if let Some(cv) = self.cv.as_deref_mut() {
            cv.stroke_rrect(r.x * s, r.y * s, r.w * s, r.h * s, radius * s, width * s, c);
        }
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

/// The look. Dark greys, one neutral accent, red only for critical.
pub mod theme {
    use crate::gfx::canvas::Color;

    pub const PANEL_BG: Color = Color::rgb(0x1f, 0x1f, 0x22);
    pub const CARD: Color = Color::rgb(0x2b, 0x2b, 0x30);
    pub const CARD_HOVER: Color = Color::rgb(0x33, 0x33, 0x39);
    pub const CONTROL: Color = Color::rgb(0x3a, 0x3a, 0x40);
    pub const CONTROL_HOVER: Color = Color::rgb(0x46, 0x46, 0x4d);
    pub const CONTROL_PRESSED: Color = Color::rgb(0x2f, 0x2f, 0x34);
    pub const TRACK: Color = Color::rgb(0x45, 0x45, 0x4b);
    pub const FILL: Color = Color::rgb(0x9a, 0x9a, 0xa2);
    pub const KNOB: Color = Color::rgb(0xe4, 0xe4, 0xe8);
    pub const TEXT: Color = Color::rgb(0xf2, 0xf2, 0xf4);
    pub const TEXT2: Color = Color::rgb(0xb8, 0xb8, 0xc0);
    pub const TEXT3: Color = Color::rgb(0x84, 0x84, 0x8d);
    pub const ART_BG: Color = Color::rgb(0x3d, 0x3d, 0x44);
    pub const CRITICAL: Color = Color::rgb(0xe5, 0x48, 0x4d);
    pub const BORDER: Color = Color::rgba(255, 255, 255, 22);
}
