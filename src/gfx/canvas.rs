//! A software canvas: premultiplied ARGB8888 (what Wayland's `argb8888` shm
//! format means on little-endian machines), with anti-aliased shapes.

use super::raster::{fill, Path};

/// Straight (non-premultiplied) colour, 0xAARRGGBB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color((a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }
    pub fn a(self) -> u32 {
        self.0 >> 24
    }
    pub fn r(self) -> u32 {
        (self.0 >> 16) & 255
    }
    pub fn g(self) -> u32 {
        (self.0 >> 8) & 255
    }
    pub fn b(self) -> u32 {
        self.0 & 255
    }
    /// The same colour with its alpha scaled by `k` (0..=1).
    pub fn fade(self, k: f32) -> Color {
        let a = (self.a() as f32 * k.clamp(0.0, 1.0) + 0.5) as u8;
        Color((self.0 & 0x00ff_ffff) | (a as u32) << 24)
    }
    /// Mix towards `other` by `t` (0 = self, 1 = other), straight alpha.
    pub fn mix(self, other: Color, t: f32) -> Color {
        let m = |a: u32, b: u32| (a as f32 + (b as f32 - a as f32) * t + 0.5) as u8;
        Color::rgba(m(self.r(), other.r()), m(self.g(), other.g()), m(self.b(), other.b()), m(self.a(), other.a()))
    }
}

#[inline]
fn mul255(a: u32, b: u32) -> u32 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// Premultiplied pixel from a straight colour scaled by `cover` (0..=255).
#[inline]
fn premul(c: Color, cover: u32) -> u32 {
    let a = mul255(c.a(), cover);
    (a << 24) | (mul255(c.r(), a) << 16) | (mul255(c.g(), a) << 8) | mul255(c.b(), a)
}

/// `src` over `dst`, both premultiplied.
#[inline]
fn over(dst: u32, src: u32) -> u32 {
    let sa = src >> 24;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let ia = 255 - sa;
    let ch = |shift: u32| -> u32 {
        let s = (src >> shift) & 255;
        let d = (dst >> shift) & 255;
        (s + mul255(d, ia)).min(255) << shift
    };
    ch(24) | ch(16) | ch(8) | ch(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x0: x, y0: y, x1: x + w, y1: y + h }
    }
    pub fn intersect(self, o: Rect) -> Rect {
        Rect { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) }
    }
    pub fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x0 as f32 && x < self.x1 as f32 && y >= self.y0 as f32 && y < self.y1 as f32
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    px: Vec<u32>,
    clip: Rect,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h], clip: Rect::new(0, 0, w as i32, h as i32) }
    }

    pub fn pixels(&self) -> &[u32] {
        &self.px
    }

    pub fn pixel(&self, x: usize, y: usize) -> u32 {
        self.px[y * self.w + x]
    }

    /// Restrict drawing to `r` (intersected with the canvas). `None` removes it.
    pub fn set_clip(&mut self, r: Option<Rect>) {
        let full = Rect::new(0, 0, self.w as i32, self.h as i32);
        self.clip = r.map(|r| r.intersect(full)).unwrap_or(full);
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    pub fn clear(&mut self, c: Color) {
        let p = premul(c, 255);
        self.px.fill(p);
    }

    /// Copy the pixels as little-endian bytes (B, G, R, A) into a shared-memory buffer.
    pub fn write_to(&self, out: &mut [u8]) {
        for (d, s) in out.chunks_exact_mut(4).zip(&self.px) {
            d.copy_from_slice(&s.to_le_bytes());
        }
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, src: u32) {
        let i = y as usize * self.w + x as usize;
        self.px[i] = over(self.px[i], src);
    }

    /// Plain filled rectangle, no anti-aliasing (integer coordinates).
    pub fn fill_rect(&mut self, r: Rect, c: Color) {
        let r = r.intersect(self.clip);
        if r.is_empty() {
            return;
        }
        let p = premul(c, 255);
        for y in r.y0..r.y1 {
            let row = &mut self.px[y as usize * self.w..(y as usize + 1) * self.w];
            if c.a() == 255 {
                row[r.x0 as usize..r.x1 as usize].fill(p);
            } else {
                for px in &mut row[r.x0 as usize..r.x1 as usize] {
                    *px = over(*px, p);
                }
            }
        }
    }

    /// Signed distance from (px,py) to a rounded rectangle (negative inside).
    #[inline]
    fn sd_rrect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
        let qx = (px - cx).abs() - (hw - r);
        let qy = (py - cy).abs() - (hh - r);
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// Anti-aliased rounded rectangle.
    pub fn fill_rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
        self.shape(x, y, w, h, r, 0.0, |d| (0.5 - d).clamp(0.0, 1.0), c);
    }

    /// Border drawn inside the rectangle's edge.
    pub fn stroke_rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: Color) {
        self.shape(
            x,
            y,
            w,
            h,
            r,
            0.0,
            move |d| (0.5 - d).clamp(0.0, 1.0) * (0.5 + d + width).clamp(0.0, 1.0),
            c,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn shape(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, grow: f32, cov: impl Fn(f32) -> f32, c: Color) {
        if w <= 0.0 || h <= 0.0 || c.a() == 0 {
            return;
        }
        let r = r.clamp(0.0, w.min(h) / 2.0);
        let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0, h / 2.0);
        let bb = Rect {
            x0: (x - grow - 1.0).floor() as i32,
            y0: (y - grow - 1.0).floor() as i32,
            x1: (x + w + grow + 1.0).ceil() as i32,
            y1: (y + h + grow + 1.0).ceil() as i32,
        }
        .intersect(self.clip);
        if bb.is_empty() {
            return;
        }
        for py in bb.y0..bb.y1 {
            for px in bb.x0..bb.x1 {
                let d = Canvas::sd_rrect(px as f32 + 0.5, py as f32 + 0.5, cx, cy, hw, hh, r);
                let k = cov(d);
                if k > 0.002 {
                    self.put(px, py, premul(c, (k * 255.0 + 0.5) as u32));
                }
            }
        }
    }

    /// Fill an arbitrary path positioned at (ox, oy).
    pub fn fill_path(&mut self, p: &Path, ox: f32, oy: f32, c: Color) {
        let Some((x0, y0, x1, y1)) = p.bounds() else { return };
        let bx = (x0 + ox).floor() as i32 - 1;
        let by = (y0 + oy).floor() as i32 - 1;
        let bw = ((x1 + ox).ceil() as i32 + 1 - bx).max(1) as usize;
        let bh = ((y1 + oy).ceil() as i32 + 1 - by).max(1) as usize;
        if bw > 4096 || bh > 4096 {
            return;
        }
        let mut moved = Path::new();
        moved.append_transformed(p, [1.0, 0.0, 0.0, 1.0, ox - bx as f32, oy - by as f32]);
        let cov = fill(&moved, bw, bh);
        self.blit_a8(bx, by, bw, bh, &cov.a, c);
    }

    /// Fill several paths as one shape: overlaps are united, so a translucent
    /// colour is not drawn twice where pieces touch.
    pub fn fill_paths(&mut self, paths: &[Path], ox: f32, oy: f32, c: Color) {
        let mut bb: Option<(f32, f32, f32, f32)> = None;
        for p in paths {
            if let Some((a, b, cc, d)) = p.bounds() {
                bb = Some(match bb {
                    None => (a, b, cc, d),
                    Some((e, f, g, h)) => (e.min(a), f.min(b), g.max(cc), h.max(d)),
                });
            }
        }
        let Some((x0, y0, x1, y1)) = bb else { return };
        let bx = (x0 + ox).floor() as i32 - 1;
        let by = (y0 + oy).floor() as i32 - 1;
        let bw = ((x1 + ox).ceil() as i32 + 1 - bx).max(1) as usize;
        let bh = ((y1 + oy).ceil() as i32 + 1 - by).max(1) as usize;
        if bw > 4096 || bh > 4096 {
            return;
        }
        let mut mask = vec![0u8; bw * bh];
        for p in paths {
            let mut moved = Path::new();
            moved.append_transformed(p, [1.0, 0.0, 0.0, 1.0, ox - bx as f32, oy - by as f32]);
            let cov = fill(&moved, bw, bh);
            for (m, v) in mask.iter_mut().zip(&cov.a) {
                // union of coverages: 1 - (1-a)(1-b)
                *m = (*m as u32 + *v as u32 - mul255(*m as u32, *v as u32)) as u8;
            }
        }
        self.blit_a8(bx, by, bw, bh, &mask, c);
    }

    /// Draw an 8-bit coverage mask tinted with `c`.
    pub fn blit_a8(&mut self, x: i32, y: i32, w: usize, h: usize, a: &[u8], c: Color) {
        let r = Rect::new(x, y, w as i32, h as i32).intersect(self.clip);
        if r.is_empty() || c.a() == 0 {
            return;
        }
        for py in r.y0..r.y1 {
            let srow = &a[(py - y) as usize * w..(py - y) as usize * w + w];
            for px in r.x0..r.x1 {
                let cov = srow[(px - x) as usize] as u32;
                if cov != 0 {
                    self.put(px, py, premul(c, cov));
                }
            }
        }
    }

    /// Draw an image given as premultiplied RGBA8 at exactly `w` x `h`,
    /// clipped to a rounded rectangle of radius `radius`.
    pub fn draw_image(&mut self, x: i32, y: i32, w: usize, h: usize, rgba_premul: &[u8], radius: f32, opacity: f32) {
        if rgba_premul.len() < w * h * 4 {
            return;
        }
        let r = Rect::new(x, y, w as i32, h as i32).intersect(self.clip);
        if r.is_empty() {
            return;
        }
        let (cx, cy) = (x as f32 + w as f32 / 2.0, y as f32 + h as f32 / 2.0);
        let radius = radius.clamp(0.0, w.min(h) as f32 / 2.0);
        let op = (opacity.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        for py in r.y0..r.y1 {
            for px in r.x0..r.x1 {
                let i = ((py - y) as usize * w + (px - x) as usize) * 4;
                let s = &rgba_premul[i..i + 4];
                if s[3] == 0 {
                    continue;
                }
                let mut k = op;
                if radius > 0.5 {
                    let d = Canvas::sd_rrect(px as f32 + 0.5, py as f32 + 0.5, cx, cy, w as f32 / 2.0, h as f32 / 2.0, radius);
                    k = mul255(k, ((0.5 - d).clamp(0.0, 1.0) * 255.0 + 0.5) as u32);
                    if k == 0 {
                        continue;
                    }
                }
                // premultiplied source scaled by k, in ARGB order
                let px32 = (mul255(s[3] as u32, k) << 24)
                    | (mul255(s[0] as u32, k) << 16)
                    | (mul255(s[1] as u32, k) << 8)
                    | mul255(s[2] as u32, k);
                self.put(px, py, px32);
            }
        }
    }
}

/// Straight RGBA8 to premultiplied RGBA8, in place.
pub fn premultiply(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 255 {
            p[0] = mul255(p[0] as u32, a) as u8;
            p[1] = mul255(p[1] as u32, a) as u8;
            p[2] = mul255(p[2] as u32, a) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_fill_and_opaque_clear() {
        let mut c = Canvas::new(4, 4);
        c.clear(Color::rgb(10, 20, 30));
        assert_eq!(c.pixel(1, 1), 0xff0a141e);
        c.fill_rect(Rect::new(1, 1, 2, 2), Color::rgb(255, 0, 0));
        assert_eq!(c.pixel(1, 1), 0xffff0000);
        assert_eq!(c.pixel(0, 0), 0xff0a141e);
    }

    #[test]
    fn alpha_blending_is_premultiplied_and_exact_at_extremes() {
        let mut c = Canvas::new(2, 1);
        c.clear(Color::rgb(0, 0, 0));
        c.fill_rect(Rect::new(0, 0, 1, 1), Color::rgba(255, 255, 255, 128));
        let p = c.pixel(0, 0);
        assert_eq!(p >> 24, 255, "opaque background stays opaque");
        let g = (p >> 8) & 255;
        assert!((127..=129).contains(&g), "{g}");
        // translucent over transparent keeps premultiplied form
        let mut t = Canvas::new(1, 1);
        t.fill_rect(Rect::new(0, 0, 1, 1), Color::rgba(255, 0, 0, 128));
        let p = t.pixel(0, 0);
        assert_eq!(p >> 24, 128);
        assert_eq!((p >> 16) & 255, 128, "red premultiplied by alpha");
    }

    #[test]
    fn rounded_rect_is_antialiased_and_clipped() {
        let mut c = Canvas::new(20, 20);
        c.fill_rrect(2.0, 2.0, 16.0, 16.0, 6.0, Color::rgb(255, 255, 255));
        assert_eq!(c.pixel(10, 10) >> 24, 255);
        assert_eq!(c.pixel(2, 2) >> 24, 0, "the corner is cut away");
        let edge = c.pixel(10, 2) >> 24;
        assert_eq!(edge, 255, "straight edge is solid");
        // some pixel along the corner arc is partially covered
        assert!((0..20).any(|i| (1..255).contains(&(c.pixel(i, 3) >> 24))));
        // fractional position gives a partial edge
        let mut c = Canvas::new(10, 4);
        c.fill_rrect(1.5, 0.0, 6.0, 4.0, 0.0, Color::rgb(255, 255, 255));
        let a = c.pixel(1, 2) >> 24;
        assert!((120..=135).contains(&a), "{a}");
    }

    #[test]
    fn clip_limits_all_drawing() {
        let mut c = Canvas::new(10, 10);
        c.set_clip(Some(Rect::new(2, 2, 4, 4)));
        c.fill_rect(Rect::new(0, 0, 10, 10), Color::rgb(255, 255, 255));
        c.fill_rrect(0.0, 0.0, 10.0, 10.0, 3.0, Color::rgb(255, 255, 255));
        c.fill_path(&Path::circle(5.0, 5.0, 20.0), 0.0, 0.0, Color::rgb(255, 255, 255));
        let filled = c.pixels().iter().filter(|&&p| p != 0).count();
        assert_eq!(filled, 16);
        c.set_clip(None);
        c.fill_rect(Rect::new(0, 0, 1, 1), Color::rgb(1, 2, 3));
        assert_ne!(c.pixel(0, 0), 0);
    }

    #[test]
    fn fill_path_and_mask_blit() {
        let mut c = Canvas::new(12, 12);
        c.fill_path(&Path::circle(6.0, 6.0, 4.0), 0.0, 0.0, Color::rgb(0, 255, 0));
        assert_eq!(c.pixel(6, 6), 0xff00ff00);
        assert_eq!(c.pixel(0, 0), 0);
        let mut m = Canvas::new(3, 1);
        m.blit_a8(0, 0, 3, 1, &[0, 128, 255], Color::rgb(255, 255, 255));
        assert_eq!(m.pixel(0, 0), 0);
        assert_eq!(m.pixel(2, 0), 0xffffffff);
        assert_eq!(m.pixel(1, 0) >> 24, 128);
    }

    #[test]
    fn overlapping_pieces_are_united_not_doubled() {
        let a = Path::polygon(&[(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)]);
        let b = Path::polygon(&[(4.0, 0.0), (12.0, 0.0), (12.0, 8.0), (4.0, 8.0)]);
        let mut c = Canvas::new(14, 10);
        c.fill_paths(&[a, b], 1.0, 1.0, Color::rgba(255, 255, 255, 100));
        let overlap = c.pixel(6, 4) >> 24;
        let single = c.pixel(2, 4) >> 24;
        assert_eq!(overlap, single, "{overlap} vs {single}");
        assert_eq!(c.pixel(11, 4) >> 24, single);
    }

    #[test]
    fn images_blend_with_rounded_mask() {
        let mut img = vec![255u8; 8 * 8 * 4]; // opaque white
        premultiply(&mut img);
        let mut c = Canvas::new(8, 8);
        c.draw_image(0, 0, 8, 8, &img, 4.0, 1.0);
        assert_eq!(c.pixel(4, 4), 0xffffffff);
        assert_eq!(c.pixel(0, 0) >> 24, 0, "rounded corner");
        let mut half = Canvas::new(8, 8);
        half.draw_image(0, 0, 8, 8, &img, 0.0, 0.5);
        let a = half.pixel(4, 4) >> 24;
        assert!((127..=129).contains(&a));
    }

    #[test]
    fn color_helpers() {
        assert_eq!(Color::rgb(1, 2, 3).0, 0xff010203);
        assert_eq!(Color::rgb(255, 255, 255).fade(0.5).a(), 128);
        assert_eq!(Color::rgb(0, 0, 0).mix(Color::rgb(100, 200, 50), 0.5), Color::rgb(50, 100, 25));
    }

    #[test]
    fn writes_little_endian_bgra() {
        let mut c = Canvas::new(1, 1);
        c.clear(Color::rgb(1, 2, 3));
        let mut b = [0u8; 4];
        c.write_to(&mut b);
        assert_eq!(b, [3, 2, 1, 255]);
    }
}
