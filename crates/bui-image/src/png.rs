//! Hand-rolled PNG decoder (RFC 2083 / W3C PNG (Second Edition)).
//!
//! Pipeline: signature → chunk iteration → IHDR → concatenate IDATs → zlib
//! inflate → defilter scanlines → expand colour-type to RGBA8.

use crate::deflate::{self, DeflateError};
use crate::Image;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PngError {
    #[error("missing PNG signature")]
    BadSignature,
    #[error("truncated input at {0}")]
    Truncated(&'static str),
    #[error("unknown critical chunk: {0:?}")]
    UnknownCriticalChunk([u8; 4]),
    #[error("missing IHDR")]
    MissingIhdr,
    #[error("missing IDAT")]
    MissingIdat,
    #[error("invalid IHDR")]
    InvalidIhdr,
    #[error("zero dimension")]
    ZeroDimension,
    #[error("unsupported bit depth {0}")]
    UnsupportedBitDepth(u8),
    #[error("unsupported color type {0}")]
    UnsupportedColorType(u8),
    #[error("unsupported compression method {0}")]
    UnsupportedCompression(u8),
    #[error("unsupported filter method {0}")]
    UnsupportedFilter(u8),
    #[error("Adam7 interlacing not yet supported")]
    InterlaceUnsupported,
    #[error("invalid filter type {0}")]
    InvalidFilterType(u8),
    #[error("filtered data wrong size: got {0}, expected {1}")]
    BadFilteredSize(usize, usize),
    #[error("PLTE chunk required for indexed colour")]
    MissingPlte,
    #[error("invalid PLTE size {0}")]
    BadPlteSize(usize),
    #[error("deflate: {0}")]
    Deflate(#[from] DeflateError),
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorType {
    Grayscale = 0,
    Rgb = 2,
    Indexed = 3,
    GrayscaleAlpha = 4,
    Rgba = 6,
}

impl ColorType {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Grayscale,
            2 => Self::Rgb,
            3 => Self::Indexed,
            4 => Self::GrayscaleAlpha,
            6 => Self::Rgba,
            _ => return None,
        })
    }

    /// Number of channels used in the *raw* (filtered) scanline.
    fn channels(self) -> usize {
        match self {
            Self::Grayscale => 1,
            Self::GrayscaleAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
            Self::Indexed => 1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: ColorType,
    interlace: u8,
}

pub fn decode(input: &[u8]) -> Result<Image, PngError> {
    if input.len() < SIGNATURE.len() || input[..SIGNATURE.len()] != SIGNATURE {
        return Err(PngError::BadSignature);
    }
    let mut cursor = SIGNATURE.len();

    let mut header: Option<Header> = None;
    let mut palette: Option<Vec<[u8; 4]>> = None;
    let mut trns_rgb: Option<[u8; 3]> = None;
    let mut trns_gray: Option<u8> = None;
    let mut idat_buf: Vec<u8> = Vec::new();

    while cursor < input.len() {
        if input.len() - cursor < 12 {
            return Err(PngError::Truncated("chunk header"));
        }
        let length = u32::from_be_bytes(input[cursor..cursor + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = input[cursor + 4..cursor + 8].try_into().unwrap();
        let data_start = cursor + 8;
        let data_end = data_start + length;
        if data_end + 4 > input.len() {
            return Err(PngError::Truncated("chunk data"));
        }
        // We don't verify CRC32 — the zlib stream and the structural
        // checks below catch the kinds of corruption we'd actually see.
        let data = &input[data_start..data_end];
        match &kind {
            b"IHDR" => {
                if data.len() != 13 {
                    return Err(PngError::InvalidIhdr);
                }
                let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let bit_depth = data[8];
                let color_type =
                    ColorType::from_u8(data[9]).ok_or(PngError::UnsupportedColorType(data[9]))?;
                let compression = data[10];
                let filter = data[11];
                let interlace = data[12];
                if width == 0 || height == 0 {
                    return Err(PngError::ZeroDimension);
                }
                if bit_depth != 8 {
                    return Err(PngError::UnsupportedBitDepth(bit_depth));
                }
                if compression != 0 {
                    return Err(PngError::UnsupportedCompression(compression));
                }
                if filter != 0 {
                    return Err(PngError::UnsupportedFilter(filter));
                }
                if interlace != 0 {
                    return Err(PngError::InterlaceUnsupported);
                }
                header = Some(Header {
                    width,
                    height,
                    bit_depth,
                    color_type,
                    interlace,
                });
            }
            b"PLTE" => {
                if data.len() % 3 != 0 || data.len() / 3 > 256 {
                    return Err(PngError::BadPlteSize(data.len()));
                }
                let mut p = Vec::with_capacity(data.len() / 3);
                for chunk in data.chunks_exact(3) {
                    p.push([chunk[0], chunk[1], chunk[2], 255]);
                }
                palette = Some(p);
            }
            b"tRNS" => {
                if let Some(h) = &header {
                    match h.color_type {
                        ColorType::Indexed => {
                            if let Some(p) = palette.as_mut() {
                                for (i, &a) in data.iter().enumerate() {
                                    if i < p.len() {
                                        p[i][3] = a;
                                    }
                                }
                            }
                        }
                        ColorType::Grayscale if data.len() >= 2 => {
                            // Grayscale tRNS is a 16-bit sample; with bit_depth=8 the high byte is 0.
                            trns_gray = Some(data[1]);
                        }
                        ColorType::Rgb if data.len() >= 6 => {
                            trns_rgb = Some([data[1], data[3], data[5]]);
                        }
                        _ => {}
                    }
                }
            }
            b"IDAT" => {
                idat_buf.extend_from_slice(data);
            }
            b"IEND" => {
                cursor = data_end + 4;
                break;
            }
            other => {
                // Unknown chunks: critical (uppercase first byte) is fatal,
                // ancillary (lowercase) is OK to skip.
                if other[0].is_ascii_uppercase() {
                    return Err(PngError::UnknownCriticalChunk(*other));
                }
            }
        }
        cursor = data_end + 4;
    }

    let header = header.ok_or(PngError::MissingIhdr)?;
    if idat_buf.is_empty() {
        return Err(PngError::MissingIdat);
    }
    if matches!(header.color_type, ColorType::Indexed) && palette.is_none() {
        return Err(PngError::MissingPlte);
    }

    let raw = deflate::inflate_zlib(&idat_buf)?;
    let unfiltered = defilter(&header, &raw)?;
    let pixels = expand_to_rgba8(&header, &unfiltered, palette.as_deref(), trns_rgb, trns_gray);

    Ok(Image {
        width: header.width,
        height: header.height,
        pixels,
    })
}

fn defilter(header: &Header, raw: &[u8]) -> Result<Vec<u8>, PngError> {
    let bpp = bytes_per_pixel(header);
    let stride = header.width as usize * bpp;
    let expected = (stride + 1) * header.height as usize;
    if raw.len() != expected {
        return Err(PngError::BadFilteredSize(raw.len(), expected));
    }
    let mut out = vec![0u8; stride * header.height as usize];
    let mut prev_row = vec![0u8; stride];
    for y in 0..header.height as usize {
        let row_start = y * (stride + 1);
        let filter = raw[row_start];
        let src = &raw[row_start + 1..row_start + 1 + stride];
        let dst_row_start = y * stride;
        match filter {
            0 => {
                out[dst_row_start..dst_row_start + stride].copy_from_slice(src);
            }
            1 => {
                for x in 0..stride {
                    let left = if x >= bpp { out[dst_row_start + x - bpp] } else { 0 };
                    out[dst_row_start + x] = src[x].wrapping_add(left);
                }
            }
            2 => {
                for x in 0..stride {
                    let up = prev_row[x];
                    out[dst_row_start + x] = src[x].wrapping_add(up);
                }
            }
            3 => {
                for x in 0..stride {
                    let left = if x >= bpp { out[dst_row_start + x - bpp] } else { 0 };
                    let up = prev_row[x];
                    let avg = ((u16::from(left) + u16::from(up)) / 2) as u8;
                    out[dst_row_start + x] = src[x].wrapping_add(avg);
                }
            }
            4 => {
                for x in 0..stride {
                    let left = if x >= bpp { out[dst_row_start + x - bpp] } else { 0 };
                    let up = prev_row[x];
                    let up_left = if x >= bpp { prev_row[x - bpp] } else { 0 };
                    out[dst_row_start + x] = src[x].wrapping_add(paeth(left, up, up_left));
                }
            }
            other => return Err(PngError::InvalidFilterType(other)),
        }
        prev_row.copy_from_slice(&out[dst_row_start..dst_row_start + stride]);
    }
    Ok(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = i32::from(a);
    let b = i32::from(b);
    let c = i32::from(c);
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    let result = if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    };
    result as u8
}

fn bytes_per_pixel(header: &Header) -> usize {
    let depth_bytes = (header.bit_depth as usize + 7) / 8;
    header.color_type.channels() * depth_bytes
}

fn expand_to_rgba8(
    header: &Header,
    src: &[u8],
    palette: Option<&[[u8; 4]]>,
    trns_rgb: Option<[u8; 3]>,
    trns_gray: Option<u8>,
) -> Vec<u8> {
    let n = (header.width * header.height) as usize;
    let mut out = Vec::with_capacity(n * 4);
    let bpp = bytes_per_pixel(header);
    for chunk in src.chunks(bpp) {
        match header.color_type {
            ColorType::Grayscale => {
                let g = chunk[0];
                let a = if Some(g) == trns_gray { 0 } else { 255 };
                out.extend_from_slice(&[g, g, g, a]);
            }
            ColorType::GrayscaleAlpha => {
                let g = chunk[0];
                let a = chunk[1];
                out.extend_from_slice(&[g, g, g, a]);
            }
            ColorType::Rgb => {
                let r = chunk[0];
                let g = chunk[1];
                let b = chunk[2];
                let a = if Some([r, g, b]) == trns_rgb { 0 } else { 255 };
                out.extend_from_slice(&[r, g, b, a]);
            }
            ColorType::Rgba => {
                out.extend_from_slice(&chunk[..4]);
            }
            ColorType::Indexed => {
                let idx = chunk[0] as usize;
                let entry = palette
                    .and_then(|p| p.get(idx))
                    .copied()
                    .unwrap_or([0, 0, 0, 255]);
                out.extend_from_slice(&entry);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_2x2_rgba() {
        // Generated by `python -c 'zlib + struct'`: 2×2 RGBA, pixels:
        //   (0,0)=red  (1,0)=green
        //   (0,1)=blue (1,1)=white
        let bytes = unhex("89504e470d0a1a0a0000000d49484452000000020000000208060000007\
                           2b60d24000000124944415478da63f8cfc0f01f0c813418000049c809f7\
                           03d964f10000000049454e44ae426082");
        let img = decode(&bytes).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 16);
        assert_eq!(&img.pixels[0..4], &[255, 0, 0, 255]); // red
        assert_eq!(&img.pixels[4..8], &[0, 255, 0, 255]); // green
        assert_eq!(&img.pixels[8..12], &[0, 0, 255, 255]); // blue
        assert_eq!(&img.pixels[12..16], &[255, 255, 255, 255]); // white
    }

    #[test]
    fn rejects_bad_signature() {
        assert!(matches!(decode(b"\x00\x00not a png"), Err(PngError::BadSignature)));
    }

    #[test]
    fn paeth_matches_spec() {
        // RFC 2083 §6.6 worked example.
        assert_eq!(paeth(10, 20, 5), 20);
        assert_eq!(paeth(10, 10, 10), 10);
        // a + b - c clamps via min-distance selection.
        assert_eq!(paeth(0, 0, 0), 0);
    }
}
