//! OpenType font variations, as far as notify-rs needs them: the `fvar`/`avar`
//! axes (to turn "weight 400" into normalized coordinates) and the Item
//! Variation Store, which CFF2 `blend` operators and `HVAR` advance widths
//! read their deltas from. TrueType `gvar` is not implemented: such fonts are
//! drawn at their default instance.

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
}
fn i16_at(d: &[u8], o: usize) -> Option<i16> {
    u16_at(d, o).map(|v| v as i16)
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*d.get(o)?, *d.get(o + 1)?, *d.get(o + 2)?, *d.get(o + 3)?]))
}
fn f2dot14(d: &[u8], o: usize) -> Option<f32> {
    i16_at(d, o).map(|v| v as f32 / 16384.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub tag: [u8; 4],
    pub min: f32,
    pub default: f32,
    pub max: f32,
}

/// The axes of an `fvar` table (empty if it is damaged).
pub fn read_axes(fvar: &[u8]) -> Vec<Axis> {
    let parse = || -> Option<Vec<Axis>> {
        let first = u16_at(fvar, 4)? as usize;
        let count = u16_at(fvar, 8)? as usize;
        let size = u16_at(fvar, 10)? as usize;
        if count > 64 || size < 20 {
            return None;
        }
        let fixed = |o: usize| u32_at(fvar, o).map(|v| v as i32 as f32 / 65536.0);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let o = first + i * size;
            let tag = [*fvar.get(o)?, *fvar.get(o + 1)?, *fvar.get(o + 2)?, *fvar.get(o + 3)?];
            let ax = Axis { tag, min: fixed(o + 4)?, default: fixed(o + 8)?, max: fixed(o + 12)? };
            if !(ax.min <= ax.default && ax.default <= ax.max) {
                return None; // nonsense axis: treat the whole table as unusable
            }
            out.push(ax);
        }
        Some(out)
    };
    parse().unwrap_or_default()
}

/// The `avar` (version 1) segment maps, one list of (from, to) per axis.
fn read_avar(avar: &[u8], axes: usize) -> Option<Vec<Vec<(f32, f32)>>> {
    if u16_at(avar, 0)? != 1 || u16_at(avar, 6)? as usize != axes {
        return None;
    }
    let mut p = 8;
    let mut out = Vec::with_capacity(axes);
    for _ in 0..axes {
        let n = u16_at(avar, p)? as usize;
        p += 2;
        let mut m = Vec::with_capacity(n);
        for _ in 0..n {
            m.push((f2dot14(avar, p)?, f2dot14(avar, p + 2)?));
            p += 4;
        }
        out.push(m);
    }
    Some(out)
}

fn piecewise(map: &[(f32, f32)], v: f32) -> f32 {
    let Some(&(first_from, first_to)) = map.first() else { return v };
    let &(last_from, last_to) = map.last().unwrap();
    if v <= first_from {
        return v - first_from + first_to;
    }
    if v >= last_from {
        return v - last_from + last_to;
    }
    for w in map.windows(2) {
        let ((f0, t0), (f1, t1)) = (w[0], w[1]);
        if v >= f0 && v <= f1 {
            return if f1 == f0 { t0 } else { t0 + (t1 - t0) * (v - f0) / (f1 - f0) };
        }
    }
    v
}

/// Normalized (-1..1) coordinates for every axis when the `wght` axis is set
/// to `weight` and all others stay at their default. `avar` is applied if given.
pub fn normalize_weight(axes: &[Axis], avar: Option<&[u8]>, weight: f32) -> Vec<f32> {
    let maps = avar.and_then(|a| read_avar(a, axes.len()));
    axes.iter()
        .enumerate()
        .map(|(i, ax)| {
            let v = if &ax.tag == b"wght" { weight.max(ax.min).min(ax.max) } else { ax.default };
            let n = if v < ax.default && ax.default > ax.min {
                (v - ax.default) / (ax.default - ax.min)
            } else if v > ax.default && ax.max > ax.default {
                (v - ax.default) / (ax.max - ax.default)
            } else {
                0.0
            };
            let n = n.clamp(-1.0, 1.0);
            match &maps {
                Some(m) => piecewise(&m[i], n).clamp(-1.0, 1.0),
                None => n,
            }
        })
        .collect()
}

/// The (min, max) of the `wght` axis, if there is one.
pub fn weight_range(axes: &[Axis]) -> Option<(f32, f32)> {
    axes.iter().find(|a| &a.tag == b"wght").map(|a| (a.min, a.max))
}

#[derive(Debug)]
struct DataSet {
    regions: Vec<u16>,
    /// absolute position of the first delta row, and its layout
    rows: usize,
    item_count: usize,
    words: usize,
    long: bool,
}

/// An Item Variation Store (regions plus delta sets), parsed lazily: the delta
/// rows stay in the font data and are read on demand.
#[derive(Debug)]
pub struct VarStore {
    /// per region, per axis: (start, peak, end)
    regions: Vec<Vec<[f32; 3]>>,
    sets: Vec<DataSet>,
}

impl VarStore {
    /// `d` is the table (or file) the store lives in and `off` its absolute start.
    pub fn parse(d: &[u8], off: usize) -> Option<VarStore> {
        if u16_at(d, off)? != 1 {
            return None;
        }
        let rl = off.checked_add(u32_at(d, off + 2)? as usize)?;
        let axis_count = u16_at(d, rl)? as usize;
        let region_count = u16_at(d, rl + 2)? as usize;
        if axis_count > 64 || region_count > 4096 {
            return None;
        }
        let mut regions = Vec::with_capacity(region_count);
        for r in 0..region_count {
            let mut axes = Vec::with_capacity(axis_count);
            for a in 0..axis_count {
                let o = rl + 4 + (r * axis_count + a) * 6;
                axes.push([f2dot14(d, o)?, f2dot14(d, o + 2)?, f2dot14(d, o + 4)?]);
            }
            regions.push(axes);
        }
        let n = u16_at(d, off + 6)? as usize;
        let mut sets = Vec::with_capacity(n);
        for i in 0..n {
            let so = off.checked_add(u32_at(d, off + 8 + 4 * i)? as usize)?;
            let item_count = u16_at(d, so)? as usize;
            let wc = u16_at(d, so + 2)?;
            let long = wc & 0x8000 != 0;
            let words = (wc & 0x7fff) as usize;
            let rc = u16_at(d, so + 4)? as usize;
            if words > rc {
                return None;
            }
            let mut idx = Vec::with_capacity(rc);
            for k in 0..rc {
                let ri = u16_at(d, so + 6 + 2 * k)?;
                if ri as usize >= region_count {
                    return None;
                }
                idx.push(ri);
            }
            sets.push(DataSet { regions: idx, rows: so + 6 + 2 * rc, item_count, words, long });
        }
        Some(VarStore { regions, sets })
    }

    pub fn data_sets(&self) -> usize {
        self.sets.len()
    }

    /// How much each region counts at the given normalized coordinates.
    pub fn region_scalars(&self, coords: &[f32]) -> Vec<f32> {
        self.regions
            .iter()
            .map(|axes| {
                let mut s = 1.0f32;
                for (i, &[start, peak, end]) in axes.iter().enumerate() {
                    let c = coords.get(i).copied().unwrap_or(0.0);
                    if peak == 0.0 || start > peak || peak > end || (start < 0.0 && end > 0.0) {
                        continue; // this axis does not take part
                    }
                    if c < start || c > end {
                        return 0.0;
                    }
                    if c == peak {
                        continue;
                    }
                    s *= if c < peak { (c - start) / (peak - start) } else { (end - c) / (end - peak) };
                }
                s
            })
            .collect()
    }

    /// The region scalars that apply to one delta set, in that set's order.
    /// This is what a CFF2 `blend` multiplies its deltas with.
    pub fn set_scalars(&self, all: &[f32], set: usize) -> Option<Vec<f32>> {
        let ds = self.sets.get(set)?;
        Some(ds.regions.iter().map(|&r| all.get(r as usize).copied().unwrap_or(0.0)).collect())
    }

    /// Summed delta for item `inner` of set `outer`.
    pub fn delta(&self, d: &[u8], all: &[f32], outer: usize, inner: usize) -> f32 {
        let Some(ds) = self.sets.get(outer) else { return 0.0 };
        if inner >= ds.item_count {
            return 0.0;
        }
        let (wsz, bsz) = if ds.long { (4, 2) } else { (2, 1) };
        let row = ds.rows + inner * (ds.words * wsz + (ds.regions.len() - ds.words) * bsz);
        let mut total = 0.0;
        for (k, &r) in ds.regions.iter().enumerate() {
            let scalar = all.get(r as usize).copied().unwrap_or(0.0);
            if scalar == 0.0 {
                continue;
            }
            let v = if k < ds.words {
                let o = row + k * wsz;
                if ds.long {
                    u32_at(d, o).map(|v| v as i32 as f32)
                } else {
                    i16_at(d, o).map(|v| v as f32)
                }
            } else {
                let o = row + ds.words * wsz + (k - ds.words) * bsz;
                if ds.long {
                    i16_at(d, o).map(|v| v as f32)
                } else {
                    d.get(o).map(|&v| v as i8 as f32)
                }
            };
            total += scalar * v.unwrap_or(0.0);
        }
        total
    }
}

/// `HVAR`: advance-width deltas.
#[derive(Debug)]
pub struct Hvar {
    store: VarStore,
    /// absolute position of the advance-width DeltaSetIndexMap, if any
    map: Option<usize>,
}

impl Hvar {
    pub fn parse(d: &[u8], off: usize) -> Option<Hvar> {
        if u16_at(d, off)? != 1 {
            return None;
        }
        let store = VarStore::parse(d, off.checked_add(u32_at(d, off + 4)? as usize)?)?;
        let map = match u32_at(d, off + 8)? {
            0 => None,
            m => Some(off.checked_add(m as usize)?),
        };
        Some(Hvar { store, map })
    }

    pub fn region_scalars(&self, coords: &[f32]) -> Vec<f32> {
        self.store.region_scalars(coords)
    }

    /// Change of the advance width of glyph `gid` at the coordinates `all` was made for.
    pub fn advance_delta(&self, d: &[u8], all: &[f32], gid: usize) -> f32 {
        let (outer, inner) = match self.map {
            None => (0, gid),
            Some(m) => match map_entry(d, m, gid) {
                Some(e) => e,
                None => return 0.0,
            },
        };
        self.store.delta(d, all, outer, inner)
    }
}

/// One entry of a DeltaSetIndexMap: (outer, inner).
fn map_entry(d: &[u8], m: usize, gid: usize) -> Option<(usize, usize)> {
    let format = *d.get(m)?;
    let entry_format = *d.get(m + 1)?;
    let (count, data) = if format == 0 { (u16_at(d, m + 2)? as usize, m + 4) } else { (u32_at(d, m + 2)? as usize, m + 6) };
    if count == 0 {
        return None;
    }
    let inner_bits = (entry_format & 0x0f) as usize + 1;
    let size = ((entry_format >> 4) & 3) as usize + 1;
    let i = gid.min(count - 1);
    let b = d.get(data + i * size..data + (i + 1) * size)?;
    let e = b.iter().fold(0usize, |a, &x| (a << 8) | x as usize);
    Some((e >> inner_bits, e & ((1 << inner_bits) - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(v: f32) -> [u8; 4] {
        ((v * 65536.0) as i32).to_be_bytes()
    }

    fn fvar(axes: &[(&[u8; 4], f32, f32, f32)]) -> Vec<u8> {
        let mut t = vec![0, 1, 0, 0, 0, 16, 0, 2];
        t.extend((axes.len() as u16).to_be_bytes());
        t.extend(20u16.to_be_bytes());
        t.extend([0, 0, 0, 0]);
        for (tag, a, b, c) in axes {
            t.extend(*tag);
            t.extend(fixed(*a));
            t.extend(fixed(*b));
            t.extend(fixed(*c));
            t.extend([0, 0, 0, 0]);
        }
        t
    }

    #[test]
    fn axes_and_weight_normalization() {
        let t = fvar(&[(b"wght", 100.0, 100.0, 900.0), (b"wdth", 75.0, 100.0, 125.0)]);
        let axes = read_axes(&t);
        assert_eq!(axes.len(), 2);
        assert_eq!(weight_range(&axes), Some((100.0, 900.0)));
        // default is the light end: 400 is 3/8 of the way up, other axes stay at 0
        let n = normalize_weight(&axes, None, 400.0);
        assert!((n[0] - 0.375).abs() < 1e-6 && n[1] == 0.0, "{n:?}");
        assert_eq!(normalize_weight(&axes, None, 900.0)[0], 1.0);
        assert_eq!(normalize_weight(&axes, None, 5000.0)[0], 1.0, "clamped");
        assert_eq!(normalize_weight(&axes, None, 100.0)[0], 0.0);
        // a regular-default font goes negative below the default
        let t = fvar(&[(b"wght", 100.0, 400.0, 900.0)]);
        let n = normalize_weight(&read_axes(&t), None, 250.0);
        assert!((n[0] + 0.5).abs() < 1e-6);
        assert!(read_axes(&[1, 2, 3]).is_empty());
        // an axis whose range is upside down makes the table unusable
        assert!(read_axes(&fvar(&[(b"wght", 900.0, 400.0, 100.0)])).is_empty());
    }

    #[test]
    fn avar_bends_the_axis() {
        let axes = read_axes(&fvar(&[(b"wght", 100.0, 100.0, 900.0)]));
        let f = |v: f32| ((v * 16384.0) as i16).to_be_bytes();
        let mut avar = vec![0, 1, 0, 0, 0, 0, 0, 1, 0, 3];
        for (a, b) in [(0.0f32, 0.0f32), (0.5, 0.75), (1.0, 1.0)] {
            avar.extend(f(a));
            avar.extend(f(b));
        }
        // 500 is halfway (0.5) and maps to 0.75
        let n = normalize_weight(&axes, Some(&avar), 500.0);
        assert!((n[0] - 0.75).abs() < 1e-3, "{n:?}");
        // between anchors it is linear
        let n = normalize_weight(&axes, Some(&avar), 300.0);
        assert!((n[0] - 0.375).abs() < 1e-3, "{n:?}");
        // a map for a different number of axes is ignored
        let n = normalize_weight(&axes, Some(&avar[..6]), 500.0);
        assert!((n[0] - 0.5).abs() < 1e-6);
    }

    /// A store with one axis and two regions: [0, 1, 1] and [0, 0.5, 1].
    fn store(long: bool) -> Vec<u8> {
        let f = |v: f32| ((v * 16384.0) as i16).to_be_bytes();
        let mut regions = vec![0, 1, 0, 2];
        for (s, p, e) in [(0.0f32, 1.0f32, 1.0f32), (0.0, 0.5, 1.0)] {
            regions.extend(f(s));
            regions.extend(f(p));
            regions.extend(f(e));
        }
        let mut set = Vec::new();
        set.extend(3u16.to_be_bytes()); // items
        set.extend((1u16 | if long { 0x8000 } else { 0 }).to_be_bytes()); // one wide delta, one narrow
        set.extend(2u16.to_be_bytes());
        set.extend([0, 0, 0, 1]); // region indexes 0, 1
        let rows: [(i32, i32); 3] = [(100, 10), (-40, -3), (1000, 0)];
        for (w, n) in rows {
            if long {
                set.extend(w.to_be_bytes());
                set.extend((n as i16).to_be_bytes());
            } else {
                set.extend((w as i16).to_be_bytes());
                set.push(n as i8 as u8);
            }
        }
        let mut t = vec![0, 1];
        t.extend(12u32.to_be_bytes()); // region list
        t.extend(1u16.to_be_bytes());
        t.extend((12 + regions.len() as u32).to_be_bytes());
        t.extend(regions);
        t.extend(set);
        t
    }

    #[test]
    fn region_scalars_and_deltas() {
        for long in [false, true] {
            let t = store(long);
            let vs = VarStore::parse(&t, 0).unwrap();
            assert_eq!(vs.data_sets(), 1);
            let s = vs.region_scalars(&[0.75]);
            assert!((s[0] - 0.75).abs() < 1e-5, "ramps up to the peak at 1");
            assert!((s[1] - 0.5).abs() < 1e-5, "past the peak at 0.5 it ramps down: (1-0.75)/(1-0.5)");
            assert_eq!(vs.region_scalars(&[0.0]), vec![0.0, 0.0]);
            assert_eq!(vs.region_scalars(&[-0.5]), vec![0.0, 0.0], "outside the region");
            assert_eq!(vs.region_scalars(&[1.0]), vec![1.0, 0.0]);
            assert_eq!(vs.set_scalars(&s, 0).unwrap(), s);
            // item 1: -40 * 0.75 + -3 * 0.5
            assert!((vs.delta(&t, &s, 0, 1) - (-31.5)).abs() < 1e-4, "long={long}");
            assert!((vs.delta(&t, &s, 0, 2) - 750.0).abs() < 1e-3);
            assert_eq!(vs.delta(&t, &s, 0, 3), 0.0, "past the end");
            assert_eq!(vs.delta(&t, &s, 1, 0), 0.0, "no such set");
        }
    }

    #[test]
    fn hvar_with_and_without_a_map() {
        let st = store(false);
        // header: 1.0, store at 20, advance map at 0 (none)
        let mut t = vec![0, 1, 0, 0];
        t.extend(20u32.to_be_bytes());
        t.extend(0u32.to_be_bytes());
        t.extend([0u8; 8]);
        t.extend(&st);
        let h = Hvar::parse(&t, 0).unwrap();
        let s = h.region_scalars(&[1.0]);
        assert_eq!(h.advance_delta(&t, &s, 0), 100.0, "glyph id is the inner index");
        assert_eq!(h.advance_delta(&t, &s, 2), 1000.0);

        // with a format-0 map: 2 entries of 1 byte, inner index in the low 2 bits; glyphs past the end repeat the last
        let mut t = vec![0, 1, 0, 0];
        t.extend(20u32.to_be_bytes());
        t.extend((20 + st.len() as u32).to_be_bytes());
        t.extend([0u8; 8]);
        t.extend(&st);
        t.extend([0, 0b0000_0001, 0, 2, 2, 0]); // entryFormat: 2 inner bits, 1 byte; entries: inner 2, inner 0
        let h = Hvar::parse(&t, 0).unwrap();
        assert_eq!(h.advance_delta(&t, &s, 0), 1000.0);
        assert_eq!(h.advance_delta(&t, &s, 1), 100.0);
        assert_eq!(h.advance_delta(&t, &s, 50), 100.0);
    }

    #[test]
    fn damaged_stores_are_refused_not_panicked_on() {
        let t = store(false);
        for cut in 0..t.len() {
            let _ = VarStore::parse(&t[..cut], 0).map(|v| v.delta(&t[..cut], &[1.0, 1.0], 0, 1));
        }
        let mut bad = t.clone();
        bad[1] = 7; // version
        assert!(VarStore::parse(&bad, 0).is_none());
        let mut bad = t.clone();
        bad[2..6].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(VarStore::parse(&bad, 0).is_none());
    }
}
