//! bui-image — PNG, JPEG, GIF decoders.
//!
//! ## Status
//!
//! Real:
//!   * Hand-rolled Deflate (RFC 1951): stored, fixed and dynamic Huffman,
//!     LZ77 back-references with run-length / overlapping copies.
//!   * Hand-rolled zlib wrapper (RFC 1950) — header validation, Adler32
//!     skipped (PNG already CRCs each chunk).
//!   * PNG decoder: signature + chunk parser, IHDR, IDAT concatenation,
//!     defilter (None / Sub / Up / Average / Paeth), expansion of
//!     8-bit grayscale / grayscale+α / RGB / RGBA / palette to RGBA8.
//!     `tRNS` honored for indexed and RGB. Adam7 interlacing not yet.
//!   * Format detection by magic bytes (`detect_format`).
//!
//! Deferred:
//!   * 16-bit PNG samples (we still parse the header but the pixel pipeline
//!     downsamples on the fly when added).
//!   * Adam7 interlacing.
//!   * JPEG decoder: marker parser, Huffman + IDCT + colour conversion.
//!     Baseline DCT only is the goal; progressive is a stretch.
//!   * GIF, WebP.

mod deflate;
mod gif;
mod jpeg;
mod png;

pub use gif::GifError;
pub use jpeg::JpegError;
pub use png::PngError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// Recognised but not decodable here — SVG is a vector format that
    /// belongs in the layout / paint pipeline (`bui-layout::svg`), not
    /// the raster image registry. Detected so callers can give a clear
    /// error message instead of "unknown image format".
    Svg,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 in row-major order.
    pub pixels: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unknown image format")]
    UnknownFormat,
    #[error("decoder for {0:?} not implemented yet")]
    NotImplemented(Format),
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("png: {0}")]
    Png(#[from] PngError),
    #[error("jpeg: {0}")]
    Jpeg(#[from] JpegError),
    #[error("gif: {0}")]
    Gif(#[from] GifError),
}

pub fn detect_format(bytes: &[u8]) -> Format {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Format::Png;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Format::Jpeg;
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Format::Gif;
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Format::Webp;
    }
    // SVG files are XML or start straight with <svg. Sniff a generous
    // prefix so an XML preamble + comments + DOCTYPE before the root
    // doesn't fool us. Whitespace-tolerant.
    let head_len = bytes.len().min(512);
    if let Ok(head) = std::str::from_utf8(&bytes[..head_len]) {
        let trimmed = head.trim_start();
        if trimmed.starts_with("<?xml") || trimmed.starts_with("<svg") || trimmed.starts_with("<!DOCTYPE svg") {
            // For "<?xml" we still need to confirm "<svg" appears in the
            // sniff window — anything else shouldn't claim to be SVG.
            if trimmed.starts_with("<svg") || head.contains("<svg") {
                return Format::Svg;
            }
        }
    }
    Format::Unknown
}

pub fn decode(bytes: &[u8]) -> Result<Image, DecodeError> {
    let fmt = detect_format(bytes);
    match fmt {
        Format::Png => Ok(png::decode(bytes)?),
        Format::Jpeg => Ok(jpeg::decode(bytes)?),
        Format::Gif => Ok(gif::decode(bytes)?),
        Format::Unknown => Err(DecodeError::UnknownFormat),
        other => Err(DecodeError::NotImplemented(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_png() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(detect_format(&png), Format::Png);
    }

    #[test]
    fn detects_svg() {
        let bare = b"<svg width='10' height='10'></svg>";
        assert_eq!(detect_format(bare), Format::Svg);
        let with_xml = b"<?xml version=\"1.0\"?><svg><path/></svg>";
        assert_eq!(detect_format(with_xml), Format::Svg);
        // Non-SVG XML doesn't get sniffed as SVG.
        let other_xml = b"<?xml version=\"1.0\"?><rss/>";
        assert_eq!(detect_format(other_xml), Format::Unknown);
    }

    #[test]
    fn detects_jpeg_and_gif() {
        assert_eq!(detect_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Format::Jpeg);
        assert_eq!(detect_format(b"GIF89a..."), Format::Gif);
    }

    #[test]
    fn unknown_returns_error() {
        let r = decode(b"\x00\x00not an image");
        assert!(matches!(r, Err(DecodeError::UnknownFormat)));
    }

    #[test]
    fn truncated_jpeg_now_errors_via_decoder_not_format_check() {
        // Just SOI + APP0 marker but no length / EOI.
        let r = decode(&[0xFF, 0xD8, 0xFF, 0xE0]);
        assert!(matches!(r, Err(DecodeError::Jpeg(_))));
    }

    #[test]
    fn decodes_real_png_via_top_level() {
        let bytes_hex = "89504e470d0a1a0a0000000d49484452000000020000000208060000007\
                         2b60d24000000124944415478da63f8cfc0f01f0c813418000049c809f7\
                         03d964f10000000049454e44ae426082";
        let bytes: Vec<u8> = (0..bytes_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&bytes_hex[i..i + 2], 16).unwrap())
            .collect();
        let img = decode(&bytes).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(&img.pixels[..4], &[255, 0, 0, 255]);
    }
}
