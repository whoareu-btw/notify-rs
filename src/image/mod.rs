//! Image decoding with no dependencies: PNG and JPEG to straight RGBA8.

pub mod inflate;
pub mod jpeg;
pub mod png;

/// Straight (non-premultiplied) RGBA8.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Decode PNG or JPEG, chosen by the file's first bytes.
pub fn decode(bytes: &[u8]) -> Result<Image, String> {
    if png::is_png(bytes) {
        png::decode(bytes)
    } else if jpeg::is_jpeg(bytes) {
        jpeg::decode(bytes)
    } else {
        Err("unsupported image format (only PNG and JPEG can be decoded)".into())
    }
}

/// Size without decoding, for PNG and JPEG.
pub fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    png::dims(bytes).or_else(|| jpeg::dims(bytes))
}
