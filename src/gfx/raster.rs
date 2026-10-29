//! Vector paths and an anti-aliased scanline rasteriser.
//!
//! The rasteriser accumulates signed area per cell (exact coverage for
//! straight edges), which is fast and needs no supersampling. Overlapping
//! contours are clamped, which is what virtually all fonts expect.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub cmds: Vec<Cmd>,
}

impl Path {
    pub fn new() -> Path {
        Path { cmds: Vec::new() }
    }
    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.cmds.push(Cmd::Move(x, y));
        self
    }
    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.cmds.push(Cmd::Line(x, y));
        self
    }
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.cmds.push(Cmd::Quad(cx, cy, x, y));
        self
    }
    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> &mut Self {
        self.cmds.push(Cmd::Cubic(c1x, c1y, c2x, c2y, x, y));
        self
    }
    pub fn close(&mut self) -> &mut Self {
        self.cmds.push(Cmd::Close);
        self
    }
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Append `other` after mapping each point through (x*sx + y*shx + dx, ...).
    pub fn append_transformed(&mut self, other: &Path, m: [f32; 6]) {
        let t = |x: f32, y: f32| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        for c in &other.cmds {
            self.cmds.push(match *c {
                Cmd::Move(x, y) => {
                    let (x, y) = t(x, y);
                    Cmd::Move(x, y)
                }
                Cmd::Line(x, y) => {
                    let (x, y) = t(x, y);
                    Cmd::Line(x, y)
                }
                Cmd::Quad(a, b, x, y) => {
                    let (a, b) = t(a, b);
                    let (x, y) = t(x, y);
                    Cmd::Quad(a, b, x, y)
                }
                Cmd::Cubic(a, b, c, d, x, y) => {
                    let (a, b) = t(a, b);
                    let (c, d) = t(c, d);
                    let (x, y) = t(x, y);
                    Cmd::Cubic(a, b, c, d, x, y)
                }
                Cmd::Close => Cmd::Close,
            });
        }
    }

    /// Rectangle with rounded corners (all four the same radius).
    pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let k = 0.552_284_75 * r; // circle approximated by cubics
        let mut p = Path::new();
        p.move_to(x + r, y)
            .line_to(x + w - r, y)
            .cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r)
            .line_to(x + w, y + h - r)
            .cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h)
            .line_to(x + r, y + h)
            .cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r)
            .line_to(x, y + r)
            .cubic_to(x, y + r - k, x + r - k, y, x + r, y)
            .close();
        p
    }

    pub fn circle(cx: f32, cy: f32, r: f32) -> Path {
        Path::rounded_rect(cx - r, cy - r, 2.0 * r, 2.0 * r, r)
    }

    /// Closed polygon through the points.
    pub fn polygon(pts: &[(f32, f32)]) -> Path {
        let mut p = Path::new();
        for (i, &(x, y)) in pts.iter().enumerate() {
            if i == 0 {
                p.move_to(x, y);
            } else {
                p.line_to(x, y);
            }
        }
        p.close();
        p
    }

    /// A stroked circular arc as a filled shape (butt ends).
    /// Angles are in radians, measured clockwise from +x in a y-down system.
    pub fn arc_stroke(cx: f32, cy: f32, r: f32, a0: f32, a1: f32, width: f32) -> Path {
        let steps = (((a1 - a0).abs() * r).ceil() as usize).clamp(6, 96);
        let (ri, ro) = (r - width / 2.0, r + width / 2.0);
        let mut pts = Vec::with_capacity(2 * steps + 2);
        for i in 0..=steps {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            pts.push((cx + ro * a.cos(), cy + ro * a.sin()));
        }
        for i in (0..=steps).rev() {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            pts.push((cx + ri * a.cos(), cy + ri * a.sin()));
        }
        Path::polygon(&pts)
    }

    /// A straight stroke as a filled quadrilateral with butt ends.
    pub fn line_stroke(x0: f32, y0: f32, x1: f32, y1: f32, width: f32) -> Path {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt().max(1e-6);
        let (nx, ny) = (-dy / len * width / 2.0, dx / len * width / 2.0);
        Path::polygon(&[(x0 + nx, y0 + ny), (x1 + nx, y1 + ny), (x1 - nx, y1 - ny), (x0 - nx, y0 - ny)])
    }

    /// Flatten to polylines (one `Vec` per contour, implicitly closed).
    /// `tol` is the allowed deviation in the path's own units.
    pub fn flatten(&self, tol: f32) -> Vec<Vec<(f32, f32)>> {
        let mut out: Vec<Vec<(f32, f32)>> = Vec::new();
        let mut cur: Vec<(f32, f32)> = Vec::new();
        let mut pen = (0.0f32, 0.0f32);
        let mut start = (0.0f32, 0.0f32);
        let finish = |cur: &mut Vec<(f32, f32)>, out: &mut Vec<Vec<(f32, f32)>>| {
            if cur.len() >= 3 {
                out.push(std::mem::take(cur));
            } else {
                cur.clear();
            }
        };
        for c in &self.cmds {
            match *c {
                Cmd::Move(x, y) => {
                    finish(&mut cur, &mut out);
                    cur.push((x, y));
                    pen = (x, y);
                    start = pen;
                }
                Cmd::Line(x, y) => {
                    if cur.is_empty() {
                        cur.push(pen);
                    }
                    cur.push((x, y));
                    pen = (x, y);
                }
                Cmd::Quad(cx, cy, x, y) => {
                    if cur.is_empty() {
                        cur.push(pen);
                    }
                    let dd = ((pen.0 - 2.0 * cx + x).powi(2) + (pen.1 - 2.0 * cy + y).powi(2)).sqrt();
                    let n = ((dd / (4.0 * tol)).sqrt().ceil() as usize).clamp(1, 64);
                    for i in 1..=n {
                        let t = i as f32 / n as f32;
                        let mt = 1.0 - t;
                        cur.push((
                            mt * mt * pen.0 + 2.0 * mt * t * cx + t * t * x,
                            mt * mt * pen.1 + 2.0 * mt * t * cy + t * t * y,
                        ));
                    }
                    pen = (x, y);
                }
                Cmd::Cubic(c1x, c1y, c2x, c2y, x, y) => {
                    if cur.is_empty() {
                        cur.push(pen);
                    }
                    let d1 = ((pen.0 - 2.0 * c1x + c2x).powi(2) + (pen.1 - 2.0 * c1y + c2y).powi(2)).sqrt();
                    let d2 = ((c1x - 2.0 * c2x + x).powi(2) + (c1y - 2.0 * c2y + y).powi(2)).sqrt();
                    let dd = d1.max(d2);
                    let n = (((3.0 * dd) / (4.0 * tol)).sqrt().ceil() as usize).clamp(1, 64);
                    for i in 1..=n {
                        let t = i as f32 / n as f32;
                        let mt = 1.0 - t;
                        let (a, b, c, d) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
                        cur.push((
                            a * pen.0 + b * c1x + c * c2x + d * x,
                            a * pen.1 + b * c1y + c * c2y + d * y,
                        ));
                    }
                    pen = (x, y);
                }
                Cmd::Close => {
                    finish(&mut cur, &mut out);
                    pen = start;
                }
            }
        }
        finish(&mut cur, &mut out);
        out
    }

    /// Signed area (after flattening); the sign follows contour direction.
    pub fn signed_area(&self) -> f32 {
        let mut a = 0.0;
        for poly in self.flatten(0.01) {
            let n = poly.len();
            for i in 0..n {
                let (x0, y0) = poly[i];
                let (x1, y1) = poly[(i + 1) % n];
                a += x0 * y1 - x1 * y0;
            }
        }
        a / 2.0
    }

    /// Bounding box of the flattened outline: (min_x, min_y, max_x, max_y).
    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let mut b: Option<(f32, f32, f32, f32)> = None;
        for poly in self.flatten(0.01) {
            for (x, y) in poly {
                b = Some(match b {
                    None => (x, y, x, y),
                    Some((a, bb, c, d)) => (a.min(x), bb.min(y), c.max(x), d.max(y)),
                });
            }
        }
        b
    }
}

/// An 8-bit coverage image.
#[derive(Clone, Debug)]
pub struct Coverage {
    pub w: usize,
    pub h: usize,
    pub a: Vec<u8>,
}

/// Rasterise `path` into a `w` x `h` coverage image. Coordinates are pixels,
/// y down, (0,0) is the top-left corner of the top-left pixel.
pub fn fill(path: &Path, w: usize, h: usize) -> Coverage {
    let mut r = Accum::new(w, h);
    for poly in path.flatten(0.08) {
        let n = poly.len();
        for i in 0..n {
            let p0 = poly[i];
            let p1 = poly[(i + 1) % n];
            r.line(p0, p1);
        }
    }
    r.finish()
}

struct Accum {
    w: usize,
    h: usize,
    stride: usize,
    a: Vec<f32>,
}

impl Accum {
    fn new(w: usize, h: usize) -> Accum {
        // one extra column absorbs edges at the right border
        let stride = w + 2;
        Accum { w, h, stride, a: vec![0.0; stride * h] }
    }

    /// Add one edge. Each scanline gets the exact area the edge covers.
    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if p0.1 == p1.1 || !(p0.0.is_finite() && p0.1.is_finite() && p1.0.is_finite() && p1.1.is_finite()) {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 { (1.0f32, p0, p1) } else { (-1.0, p1, p0) };
        if p1.1 <= 0.0 || p0.1 >= self.h as f32 {
            return;
        }
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let mut y = p0.1;
        if y < 0.0 {
            x -= y * dxdy;
            y = 0.0;
        }
        let y_end = p1.1.min(self.h as f32);
        let wf = self.w as f32;
        let mut row = y.floor() as usize;
        while (row as f32) < y_end && row < self.h {
            let top = y.max(row as f32);
            let bot = ((row + 1) as f32).min(y_end);
            let dy = bot - top;
            if dy <= 0.0 {
                row += 1;
                continue;
            }
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            // clamp to the buffer; area left of x=0 still counts fully at column 0
            let (xa, xb) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let xa = xa.clamp(0.0, wf);
            let xb = xb.clamp(0.0, wf);
            let base = row * self.stride;
            let x0i = xa.floor() as usize;
            let x1i = xb.ceil() as usize;
            if x1i <= x0i + 1 {
                // edge stays inside one cell
                let xmf = 0.5 * (xa + xb) - x0i as f32;
                self.a[base + x0i] += d - d * xmf;
                self.a[base + x0i + 1] += d * xmf;
            } else {
                let s = 1.0 / (xb - xa);
                let x0f = xa - x0i as f32;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = xb - x1i as f32 + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.a[base + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.a[base + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.a[base + x0i + 1] += d * (a1 - a0);
                    for xi in (x0i + 2)..(x1i - 1) {
                        self.a[base + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.a[base + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.a[base + x1i] += d * am;
            }
            x += dxdy * dy;
            y = bot;
            row += 1;
        }
    }

    fn finish(self) -> Coverage {
        let mut out = vec![0u8; self.w * self.h];
        for y in 0..self.h {
            let mut acc = 0.0f32;
            let row = &self.a[y * self.stride..y * self.stride + self.w];
            let dst = &mut out[y * self.w..(y + 1) * self.w];
            for (o, v) in dst.iter_mut().zip(row) {
                acc += v;
                *o = (acc.abs().min(1.0) * 255.0 + 0.5) as u8;
            }
        }
        Coverage { w: self.w, h: self.h, a: out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(c: &Coverage) -> f32 {
        c.a.iter().map(|&v| v as f32 / 255.0).sum()
    }

    #[test]
    fn pixel_aligned_square_is_exact() {
        let p = Path::polygon(&[(2.0, 2.0), (6.0, 2.0), (6.0, 5.0), (2.0, 5.0)]);
        let c = fill(&p, 8, 8);
        assert_eq!(c.a[3 * 8 + 3], 255);
        assert_eq!(c.a[1 * 8 + 3], 0);
        assert_eq!(c.a[3 * 8 + 6], 0);
        assert!((total(&c) - 12.0).abs() < 0.05);
    }

    #[test]
    fn fractional_edges_give_partial_coverage() {
        let p = Path::polygon(&[(1.5, 1.0), (4.0, 1.0), (4.0, 3.0), (1.5, 3.0)]);
        let c = fill(&p, 6, 4);
        assert!((c.a[1 * 6 + 1] as i32 - 128).abs() <= 1, "{}", c.a[1 * 6 + 1]);
        assert_eq!(c.a[1 * 6 + 2], 255);
        assert!((total(&c) - 5.0).abs() < 0.1);
    }

    #[test]
    fn area_matches_geometry_for_circles_and_triangles() {
        let c = fill(&Path::circle(20.0, 20.0, 15.0), 40, 40);
        let want = std::f32::consts::PI * 15.0 * 15.0;
        assert!((total(&c) - want).abs() / want < 0.005, "{} vs {want}", total(&c));
        let tri = Path::polygon(&[(2.3, 1.1), (30.7, 10.9), (9.2, 28.4)]);
        let c = fill(&tri, 32, 32);
        let want = tri.signed_area().abs();
        assert!((total(&c) - want).abs() / want < 0.005, "{} vs {want}", total(&c));
    }

    #[test]
    fn winding_direction_does_not_matter_for_a_single_shape() {
        let cw = Path::polygon(&[(2.0, 2.0), (10.0, 2.0), (10.0, 10.0), (2.0, 10.0)]);
        let ccw = Path::polygon(&[(2.0, 2.0), (2.0, 10.0), (10.0, 10.0), (10.0, 2.0)]);
        assert_eq!(fill(&cw, 12, 12).a, fill(&ccw, 12, 12).a);
    }

    #[test]
    fn opposite_contour_makes_a_hole() {
        let mut p = Path::polygon(&[(1.0, 1.0), (11.0, 1.0), (11.0, 11.0), (1.0, 11.0)]);
        let hole = Path::polygon(&[(4.0, 4.0), (4.0, 8.0), (8.0, 8.0), (8.0, 4.0)]);
        p.cmds.extend(hole.cmds);
        let c = fill(&p, 12, 12);
        assert_eq!(c.a[6 * 12 + 6], 0, "hole");
        assert_eq!(c.a[2 * 12 + 2], 255);
    }

    #[test]
    fn shapes_partly_or_wholly_outside_are_clipped_safely() {
        // far outside, straddling every border, and absurd values
        for pts in [
            vec![(-50.0, -50.0), (-10.0, -50.0), (-10.0, -10.0)],
            vec![(-5.0, -5.0), (20.0, -5.0), (20.0, 20.0), (-5.0, 20.0)],
            vec![(5.0, -1e9), (6.0, 1e9), (7.0, 0.0)],
            vec![(f32::NAN, 1.0), (3.0, 3.0), (1.0, 5.0)],
            vec![(1.0, 1.0), (f32::INFINITY, 2.0), (3.0, 4.0)],
        ] {
            let c = fill(&Path::polygon(&pts), 10, 10);
            assert_eq!(c.a.len(), 100);
        }
        let c = fill(&Path::polygon(&[(-5.0, -5.0), (20.0, -5.0), (20.0, 20.0), (-5.0, 20.0)]), 10, 10);
        assert!(c.a.iter().all(|&v| v == 255), "a shape covering the whole image fills it");
    }

    #[test]
    fn rounded_rect_corners_are_cut() {
        let c = fill(&Path::rounded_rect(1.0, 1.0, 18.0, 18.0, 6.0), 20, 20);
        assert!(c.a[1 * 20 + 1] < 20, "corner pixel is mostly empty: {}", c.a[21]);
        assert_eq!(c.a[10 * 20 + 10], 255);
        assert_eq!(c.a[1 * 20 + 10], 255, "straight edge is solid");
    }

    #[test]
    fn strokes_have_the_expected_area() {
        let s = Path::line_stroke(2.0, 5.0, 12.0, 5.0, 2.0);
        assert!((s.signed_area().abs() - 20.0).abs() < 0.01);
        let a = Path::arc_stroke(10.0, 10.0, 6.0, 0.0, std::f32::consts::PI, 2.0);
        let want = std::f32::consts::PI * 6.0 * 2.0; // half circle, length * width
        assert!((a.signed_area().abs() - want).abs() / want < 0.02, "{}", a.signed_area());
    }

    #[test]
    fn transform_and_bounds() {
        let mut p = Path::new();
        p.append_transformed(&Path::polygon(&[(0.0, 0.0), (2.0, 0.0), (2.0, 1.0)]), [2.0, 0.0, 0.0, 3.0, 10.0, 20.0]);
        let (x0, y0, x1, y1) = p.bounds().unwrap();
        assert_eq!((x0, y0, x1, y1), (10.0, 20.0, 14.0, 23.0));
    }
}
