//! Album art lookup. `file://` art is used in place. `http(s)` art goes through
//! a small cache under `~/.cache/notify-rs`, filled by `fetch`, which runs
//! `curl` if it is installed (notify-rs has no TLS of its own, and nothing
//! else needs the network).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{cache_dir, cache_key, file_uri_to_path};

/// The art if it is already on disk (a local file or a cached download).
/// Never touches the network, so it is safe on any thread.
pub fn local_path(url: &str) -> Option<PathBuf> {
    if let Some(p) = file_uri_to_path(url) {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let p = cache_dir()?.join(cache_key(url));
    p.exists().then_some(p)
}

/// Download `url` into the cache with `curl` and return the file. Blocks for up
/// to ~10 s, so call it from a worker thread. None if curl is missing, the
/// request fails, or what came back is not an image we can decode.
pub fn fetch(url: &str) -> Option<PathBuf> {
    if let Some(p) = local_path(url) {
        return Some(p);
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2048 || url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return None;
    }
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    prune(&dir, 200);
    let key = cache_key(url);
    let part = dir.join(format!("{key}.{}.part", std::process::id()));
    let ok = Command::new("curl")
        .args(["--silent", "--fail", "--location", "--max-redirs", "3", "--proto", "=http,https", "--proto-redir", "=http,https"])
        .args(["--connect-timeout", "5", "--max-time", "10", "--max-filesize", "16777216", "--output"])
        .arg(&part)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let dest = dir.join(&key);
    let good = ok && std::fs::read(&part).ok().is_some_and(|b| crate::image::dims(&b).is_some());
    if good && std::fs::rename(&part, &dest).is_ok() {
        return Some(dest);
    }
    let _ = std::fs::remove_file(&part);
    None
}

/// Keep the cache from growing without bound: past `max` files, drop the oldest half.
fn prune(dir: &Path, max: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= max {
        return;
    }
    files.sort();
    for (_, p) in files.iter().take(files.len() / 2) {
        let _ = std::fs::remove_file(p);
    }
}

/// Decode PNG/JPEG bytes into a `dev` x `dev` square (centre-cropped) of
/// premultiplied RGBA, ready for `Canvas::draw_image`.
pub fn square_from_bytes(bytes: &[u8], dev: usize) -> Option<Vec<u8>> {
    let img = crate::image::decode(bytes).ok()?;
    Some(square_from_rgba(&img.rgba, img.w, img.h, dev))
}

/// Straight RGBA8 pixels to a premultiplied `dev` x `dev` square.
pub fn square_from_rgba(rgba: &[u8], w: usize, h: usize, dev: usize) -> Vec<u8> {
    let mut px = rgba.to_vec();
    crate::gfx::canvas::premultiply(&mut px);
    crate::scale::resample_square(&px, w, h, w * 4, dev)
}

/// Art or icon from a `file://` URL, an absolute path or (via `curl`) a web
/// address. May block on the network, so it belongs on a worker thread.
pub fn load_square(source: &str, dev: usize) -> Option<Vec<u8>> {
    let path = if source.starts_with('/') { Some(PathBuf::from(source)).filter(|p| p.exists()) } else { local_path(source).or_else(|| fetch(source)) }?;
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > 32 * 1024 * 1024 {
        return None;
    }
    square_from_bytes(&std::fs::read(path).ok()?, dev)
}

/// Can `load_square` do anything with this string? (icon *names* are not resolved)
pub fn loadable(source: &str) -> bool {
    source.starts_with('/') || source.starts_with("file://") || source.starts_with("http://") || source.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squares_are_premultiplied_and_sized() {
        // 4x2 opaque red + half transparent green
        let mut rgba = vec![0u8; 4 * 2 * 4];
        for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
            if i % 4 < 2 {
                px.copy_from_slice(&[255, 0, 0, 255]);
            } else {
                px.copy_from_slice(&[0, 255, 0, 128]);
            }
        }
        let out = square_from_rgba(&rgba, 4, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 4);
        assert!(out.chunks_exact(4).all(|p| p[3] > 100), "{out:?}");
    }

    #[test]
    fn loading_missing_or_wrong_things_is_none() {
        assert!(load_square("/nonexistent/art.png", 16).is_none());
        assert!(load_square("not a url", 16).is_none());
        let dir = std::env::temp_dir().join(format!("notify-rs-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("junk.png");
        std::fs::write(&f, b"definitely not an image").unwrap();
        assert!(load_square(f.to_str().unwrap(), 16).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(loadable("/a/b") && loadable("file:///a") && !loadable("mail-unread"));
    }
}
