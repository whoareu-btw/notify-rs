//! Image helpers with no dependencies: a high-quality resampler for the album
//! art (so it is crisp instead of softly scaled by the toolkit) and a tiny
//! reader for image sizes (used by `notify-rs art`).

use std::f32::consts::PI;

fn lanczos3(x: f32) -> f32 {
    let x = x.abs();
    if x >= 3.0 {
        0.0
    } else if x < 1e-6 {
        1.0
    } else {
        let px = PI * x;
        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
    }
}

/// For each destination index: (first source index, normalised weights).
/// The kernel widens when shrinking, which is what makes it anti-aliased.
fn weights(src_len: usize, dst_len: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src_len as f32 / dst_len as f32;
    let filter_scale = scale.max(1.0);
    let support = 3.0 * filter_scale;
    (0..dst_len)
        .map(|i| {
            let center = (i as f32 + 0.5) * scale;
            let start = (center - support).floor().max(0.0) as usize;
            let end = ((center + support).ceil() as usize).min(src_len).max(start + 1);
            let mut w: Vec<f32> = (start..end)
                .map(|j| lanczos3(((j as f32 + 0.5) - center) / filter_scale))
                .collect();
            let sum: f32 = w.iter().sum();
            if sum.abs() > 1e-6 {
                for v in &mut w {
                    *v /= sum;
                }
            } else {
                w = vec![1.0];
            }
            (start, w)
        })
        .collect()
}

/// Centre-crop `src` (4 bytes per pixel, rows `stride` bytes apart) to a
/// square and resample it to `dst` x `dst` with a Lanczos-3 filter.
/// Works on any 4-channel layout (premultiplied BGRA/RGBA alike).
pub fn resample_square(src: &[u8], sw: usize, sh: usize, stride: usize, dst: usize) -> Vec<u8> {
    let mut out = vec![0u8; dst * dst * 4];
    if sw == 0 || sh == 0 || dst == 0 || stride < sw * 4 || src.len() < stride * (sh - 1) + sw * 4 {
        return out;
    }
    let side = sw.min(sh);
    let (x0, y0) = ((sw - side) / 2, (sh - side) / 2);

    // horizontal pass: side rows x dst columns
    let wx = weights(side, dst);
    let mut tmp = vec![0f32; side * dst * 4];
    for y in 0..side {
        let row = &src[(y0 + y) * stride..];
        for (dx, (start, w)) in wx.iter().enumerate() {
            let mut acc = [0f32; 4];
            for (k, wk) in w.iter().enumerate() {
                let p = (x0 + start + k) * 4;
                for c in 0..4 {
                    acc[c] += row[p + c] as f32 * wk;
                }
            }
            let o = (y * dst + dx) * 4;
            tmp[o..o + 4].copy_from_slice(&acc);
        }
    }

    // vertical pass: dst x dst
    let wy = weights(side, dst);
    for (dy, (start, w)) in wy.iter().enumerate() {
        for dx in 0..dst {
            let mut acc = [0f32; 4];
            for (k, wk) in w.iter().enumerate() {
                let o = ((start + k) * dst + dx) * 4;
                for c in 0..4 {
                    acc[c] += tmp[o + c] * wk;
                }
            }
            let o = (dy * dst + dx) * 4;
            for c in 0..4 {
                out[o + c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// (width, height) from the first bytes of a PNG, JPEG, GIF or WebP file.
pub fn image_dims(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    // PNG: signature, then the IHDR chunk
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be32(16), be32(20)));
    }
    // GIF
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some((u16::from_le_bytes([b[6], b[7]]) as u32, u16::from_le_bytes([b[8], b[9]]) as u32));
    }
    // JPEG: walk the segments until a start-of-frame marker
    if b.len() >= 4 && b[0] == 0xff && b[1] == 0xd8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if m == 0xff {
                i += 1;
                continue;
            }
            if (0xc0..=0xcf).contains(&m) && m != 0xc4 && m != 0xc8 && m != 0xcc {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some((w, h));
            }
            if m == 0xd8 || m == 0x01 || (0xd0..=0xd7).contains(&m) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            i += 2 + len;
        }
        return None;
    }
    // WebP: lossy (VP8 ), lossless (VP8L) or extended (VP8X)
    if b.len() >= 30 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        match &b[12..16] {
            b"VP8 " => {
                let w = (u16::from_le_bytes([b[26], b[27]]) & 0x3fff) as u32;
                let h = (u16::from_le_bytes([b[28], b[29]]) & 0x3fff) as u32;
                return Some((w, h));
            }
            b"VP8L" => {
                let v = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                return Some(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1));
            }
            b"VP8X" => {
                let w = u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1;
                let h = u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1;
                return Some((w, h));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, px: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| px).collect()
    }

    #[test]
    fn constant_image_stays_constant() {
        let src = solid(37, 37, [10, 120, 200, 255]);
        for dst in [8usize, 20, 37, 80] {
            let out = resample_square(&src, 37, 37, 37 * 4, dst);
            assert_eq!(out.len(), dst * dst * 4);
            assert!(out.chunks(4).all(|p| p == [10, 120, 200, 255]), "dst {dst}");
        }
    }

    #[test]
    fn same_size_is_identity() {
        let src: Vec<u8> = (0..16 * 16 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let out = resample_square(&src, 16, 16, 16 * 4, 16);
        assert_eq!(out, src);
    }

    #[test]
    fn downscale_averages_instead_of_aliasing() {
        // alternating black / white columns: a good filter gives mid grey
        let (w, h) = (64usize, 64usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if x % 2 == 0 { 0 } else { 255 };
                src[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let out = resample_square(&src, w, h, w * 4, 16);
        for (i, p) in out.chunks(4).enumerate() {
            let (x, y) = (i % 16, i / 16);
            let edge = x == 0 || y == 0 || x == 15 || y == 15;
            // inside: mid grey. At the very border the filter is cut off by
            // the image edge, which this worst-case pattern exaggerates.
            let tol = if edge { 20 } else { 4 };
            assert!((p[0] as i32 - 128).abs() <= tol, "({x},{y}) got {}", p[0]);
            assert_eq!(p[3], 255);
        }
    }

    #[test]
    fn non_square_is_centre_cropped() {
        // 6 wide x 2 tall: left third red, middle blue, right third green
        let (w, h) = (6usize, 2usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = if x < 2 { [255, 0, 0, 255] } else if x < 4 { [0, 0, 255, 255] } else { [0, 255, 0, 255] };
                src[(y * w + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        // the centred 2x2 square is the blue block
        let out = resample_square(&src, w, h, w * 4, 2);
        assert!(out.chunks(4).all(|p| p == [0, 0, 255, 255]));
    }

    #[test]
    fn respects_stride_padding() {
        let (w, h, stride) = (4usize, 4usize, 4 * 4 + 8);
        let mut src = vec![77u8; stride * h]; // padding bytes are 77 too
        for y in 0..h {
            for x in 0..w {
                src[y * stride + x * 4..][..4].copy_from_slice(&[1, 2, 3, 255]);
            }
        }
        let out = resample_square(&src, w, h, stride, 2);
        assert!(out.chunks(4).all(|p| p == [1, 2, 3, 255]));
    }

    #[test]
    fn bad_input_gives_blank_not_panic() {
        assert_eq!(resample_square(&[], 0, 0, 0, 4).len(), 64);
        assert_eq!(resample_square(&[0; 10], 4, 4, 16, 4).len(), 64);
    }

    #[test]
    fn dims_png_gif_jpeg_webp() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&300u32.to_be_bytes());
        png.extend_from_slice(&200u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(image_dims(&png), Some((300, 200)));

        let gif = [b"GIF89a".as_slice(), &[44, 1, 90, 0]].concat();
        assert_eq!(image_dims(&gif), Some((300, 90)));

        // SOI, APP0 (len 16), SOF0 with height 120 / width 480
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        jpg.extend_from_slice(&[0u8; 14]);
        jpg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8, 0, 120, 1, 224, 3]);
        jpg.extend_from_slice(&[0u8; 12]);
        assert_eq!(image_dims(&jpg), Some((480, 120)));

        // WebP lossless 64 x 32 -> stored as (w-1) | (h-1) << 14
        let v: u32 = 63 | (31 << 14);
        let mut webp = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
        webp.extend_from_slice(&v.to_le_bytes());
        webp.extend_from_slice(&[0u8; 8]);
        assert_eq!(image_dims(&webp), Some((64, 32)));

        assert_eq!(image_dims(b"not an image at all, sorry......"), None);
        assert_eq!(image_dims(&[]), None);
    }
}
