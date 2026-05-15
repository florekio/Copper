//! GIF decoder — first-frame static decode.
//!
//! Coverage:
//!   * GIF87a / GIF89a header.
//!   * Logical Screen Descriptor + Global Color Table.
//!   * Image Descriptor + optional Local Color Table.
//!   * LZW-compressed image data with the standard variable-width
//!     code stream (clear / end-of-information / KwKwK).
//!   * Interlaced rows reordered to row-major before output.
//!   * Graphic Control Extension's transparent-color flag honoured.
//!   * Application / Comment / Plain-Text extensions skipped.
//!
//! Out of scope:
//!   * Animation. Animated GIFs decode their first frame and stop —
//!     the rest of the engine has no notion of timed frames yet.
//!   * Disposal methods, frame compositing.
//!
//! Output is RGBA8 in row-major order, padded out to the logical
//! screen size when the first frame is smaller (uncovered areas get
//! the background-color or transparent, depending on the GCE).

use crate::Image;

#[derive(Debug, thiserror::Error)]
pub enum GifError {
    #[error("malformed GIF: {0}")]
    Malformed(String),
    #[error("unexpected end of stream")]
    Eof,
}

pub fn decode(bytes: &[u8]) -> Result<Image, GifError> {
    let mut p = Parser::new(bytes);
    // Header: "GIF87a" or "GIF89a".
    let sig = p.read_n(6)?;
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return Err(GifError::Malformed(format!(
            "bad signature: {:?}",
            std::str::from_utf8(sig).unwrap_or("?")
        )));
    }
    // Logical Screen Descriptor.
    let lsd = p.read_n(7)?;
    let screen_w = u16::from_le_bytes([lsd[0], lsd[1]]) as usize;
    let screen_h = u16::from_le_bytes([lsd[2], lsd[3]]) as usize;
    let packed = lsd[4];
    let bg_index = lsd[5] as usize;
    let _ = lsd[6]; // pixel-aspect ratio, ignored.

    let gct_present = (packed & 0x80) != 0;
    let gct_size = 1usize << (((packed & 0x07) + 1) as usize);
    let gct = if gct_present {
        let raw = p.read_n(3 * gct_size)?;
        let mut palette = vec![[0u8; 3]; gct_size];
        for (i, chunk) in raw.chunks(3).enumerate() {
            palette[i] = [chunk[0], chunk[1], chunk[2]];
        }
        Some(palette)
    } else {
        None
    };

    // Default to opaque background; overridden if a GCE marks a
    // transparent index.
    let mut transparent_index: Option<usize> = None;

    // Walk extension/image blocks until we find an image descriptor or
    // the trailer.
    loop {
        let intro = p.read_u8()?;
        match intro {
            0x3B => return Err(GifError::Malformed("trailer before image data".into())),
            0x21 => {
                // Extension. Label byte tells us which.
                let label = p.read_u8()?;
                if label == 0xF9 {
                    // Graphic Control Extension. Block size = 4.
                    let size = p.read_u8()?;
                    if size != 4 {
                        return Err(GifError::Malformed("GCE size != 4".into()));
                    }
                    let body = p.read_n(4)?;
                    let pkg = body[0];
                    if pkg & 0x01 != 0 {
                        transparent_index = Some(body[3] as usize);
                    }
                    // Block terminator.
                    let term = p.read_u8()?;
                    if term != 0 {
                        return Err(GifError::Malformed("GCE terminator".into()));
                    }
                } else {
                    // Skip every sub-block of the unknown extension.
                    skip_subblocks(&mut p)?;
                }
            }
            0x2C => {
                // Image Descriptor — 9 bytes.
                let header = p.read_n(9)?;
                let frame_left = u16::from_le_bytes([header[0], header[1]]) as usize;
                let frame_top = u16::from_le_bytes([header[2], header[3]]) as usize;
                let frame_w = u16::from_le_bytes([header[4], header[5]]) as usize;
                let frame_h = u16::from_le_bytes([header[6], header[7]]) as usize;
                let packed = header[8];
                let lct_present = (packed & 0x80) != 0;
                let interlaced = (packed & 0x40) != 0;
                let lct_size = 1usize << (((packed & 0x07) + 1) as usize);
                let lct = if lct_present {
                    let raw = p.read_n(3 * lct_size)?;
                    let mut palette = vec![[0u8; 3]; lct_size];
                    for (i, chunk) in raw.chunks(3).enumerate() {
                        palette[i] = [chunk[0], chunk[1], chunk[2]];
                    }
                    Some(palette)
                } else {
                    None
                };
                let palette = lct
                    .as_deref()
                    .or(gct.as_deref())
                    .ok_or_else(|| GifError::Malformed("no palette available".into()))?;

                // LZW decode.
                let lzw_min = p.read_u8()? as u32;
                let mut compressed: Vec<u8> = Vec::new();
                loop {
                    let len = p.read_u8()? as usize;
                    if len == 0 {
                        break;
                    }
                    compressed.extend_from_slice(p.read_n(len)?);
                }
                let frame_pixels = lzw_decode(&compressed, lzw_min, frame_w * frame_h)?;
                // De-interlace if needed.
                let frame_pixels = if interlaced {
                    deinterlace(&frame_pixels, frame_w, frame_h)
                } else {
                    frame_pixels
                };

                // Composite the frame onto a screen-sized canvas. Areas
                // outside the frame get the background colour from the
                // GCT (or transparent if no GCT).
                let mut out = vec![0u8; screen_w * screen_h * 4];
                if let Some(palette) = gct.as_deref() {
                    if bg_index < palette.len() && transparent_index != Some(bg_index) {
                        let [r, g, b] = palette[bg_index];
                        for px in out.chunks_exact_mut(4) {
                            px.copy_from_slice(&[r, g, b, 255]);
                        }
                    }
                }
                for fy in 0..frame_h {
                    for fx in 0..frame_w {
                        let sx = frame_left + fx;
                        let sy = frame_top + fy;
                        if sx >= screen_w || sy >= screen_h {
                            continue;
                        }
                        let idx = frame_pixels[fy * frame_w + fx] as usize;
                        let i = (sy * screen_w + sx) * 4;
                        if Some(idx) == transparent_index {
                            out[i + 3] = 0;
                            continue;
                        }
                        if idx >= palette.len() {
                            continue;
                        }
                        let [r, g, b] = palette[idx];
                        out[i] = r;
                        out[i + 1] = g;
                        out[i + 2] = b;
                        out[i + 3] = 255;
                    }
                }
                return Ok(Image {
                    width: screen_w as u32,
                    height: screen_h as u32,
                    pixels: out,
                });
            }
            _ => {
                return Err(GifError::Malformed(format!(
                    "unknown block intro {:#04X}",
                    intro
                )))
            }
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn read_u8(&mut self) -> Result<u8, GifError> {
        let b = *self.bytes.get(self.pos).ok_or(GifError::Eof)?;
        self.pos += 1;
        Ok(b)
    }
    fn read_n(&mut self, n: usize) -> Result<&'a [u8], GifError> {
        let start = self.pos;
        let end = start + n;
        if end > self.bytes.len() {
            return Err(GifError::Eof);
        }
        self.pos = end;
        Ok(&self.bytes[start..end])
    }
}

fn skip_subblocks(p: &mut Parser<'_>) -> Result<(), GifError> {
    loop {
        let len = p.read_u8()? as usize;
        if len == 0 {
            return Ok(());
        }
        let _ = p.read_n(len)?;
    }
}

/// Decode a GIF LZW stream into a flat index array of length
/// `expected_pixels`. The dictionary grows from `2^min + 2` (where the
/// last two slots are clear and end-of-information) up to 4096; on
/// overflow further codes are emitted using the dictionary frozen at
/// that size, until a clear-code resets it.
fn lzw_decode(data: &[u8], min_code_size: u32, expected: usize) -> Result<Vec<u8>, GifError> {
    if min_code_size < 2 || min_code_size > 8 {
        return Err(GifError::Malformed("LZW min code size out of range".into()));
    }
    let clear = 1u32 << min_code_size;
    let end_of_info = clear + 1;
    let mut code_size = min_code_size + 1;
    let mut next_code = end_of_info + 1;

    // dict_strings[i] holds the bytes for code `i`. Codes 0..clear are
    // single-byte literals; clear and end_of_info hold no data.
    let mut dict: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let init_dict = |dict: &mut Vec<Vec<u8>>| {
        dict.clear();
        for i in 0..clear {
            dict.push(vec![i as u8]);
        }
        dict.push(Vec::new()); // clear
        dict.push(Vec::new()); // end_of_info
    };
    init_dict(&mut dict);

    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut bit_buf: u32 = 0;
    let mut bit_len: u32 = 0;
    let mut byte_pos = 0usize;
    let mut prev_code: Option<u32> = None;
    loop {
        while bit_len < code_size {
            if byte_pos >= data.len() {
                // No more bits — finish quietly. Many real GIFs omit
                // the end-of-information code.
                return Ok(out);
            }
            bit_buf |= (data[byte_pos] as u32) << bit_len;
            bit_len += 8;
            byte_pos += 1;
        }
        let code = bit_buf & ((1 << code_size) - 1);
        bit_buf >>= code_size;
        bit_len -= code_size;

        if code == clear {
            init_dict(&mut dict);
            code_size = min_code_size + 1;
            next_code = end_of_info + 1;
            prev_code = None;
            continue;
        }
        if code == end_of_info {
            return Ok(out);
        }
        let entry: Vec<u8> = if (code as usize) < dict.len() {
            dict[code as usize].clone()
        } else if code == next_code {
            // KwKwK: code == next dictionary slot. Emit prev + prev[0].
            let p = prev_code.ok_or_else(|| GifError::Malformed("LZW: KwKwK before prev".into()))?;
            let mut s = dict[p as usize].clone();
            let first = *s
                .first()
                .ok_or_else(|| GifError::Malformed("LZW: empty prev".into()))?;
            s.push(first);
            s
        } else {
            return Err(GifError::Malformed(format!(
                "LZW: code {} out of range",
                code
            )));
        };
        out.extend_from_slice(&entry);
        if let Some(p) = prev_code {
            // Add prev + entry[0] to the dictionary.
            if dict.len() < 4096 {
                let mut new_entry = dict[p as usize].clone();
                new_entry.push(entry[0]);
                dict.push(new_entry);
                if dict.len() == (1 << code_size) as usize && code_size < 12 {
                    code_size += 1;
                }
                next_code = dict.len() as u32;
            }
        }
        prev_code = Some(code);
        if out.len() > expected * 4 {
            // Pathological / corrupt stream — bail out before we
            // explode memory.
            return Err(GifError::Malformed("LZW output exceeds frame size".into()));
        }
    }
}

/// Reorder GIF interlaced rows back to row-major. Pass 1: rows 0, 8,
/// 16, ...; Pass 2: rows 4, 12, ...; Pass 3: rows 2, 6, 10, ...;
/// Pass 4: rows 1, 3, 5, ....
fn deinterlace(src: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let passes: [(usize, usize); 4] = [(0, 8), (4, 8), (2, 4), (1, 2)];
    let mut src_row = 0usize;
    for (start, step) in passes {
        let mut y = start;
        while y < height {
            let dst_off = y * width;
            let src_off = src_row * width;
            out[dst_off..dst_off + width].copy_from_slice(&src[src_off..src_off + width]);
            src_row += 1;
            y += step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 GIF89a, single black pixel. Hand-assembled.
    fn black_pixel_gif() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        // LSD: 1x1, GCT flag, 1-bit color resolution, GCT size = 0 (2 entries)
        bytes.extend_from_slice(&[
            0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00,
        ]);
        // GCT: black + white (2 entries × 3 bytes).
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF]);
        // Image Descriptor: at (0,0), 1x1, no LCT, not interlaced.
        bytes.extend_from_slice(&[0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
        // LZW: min code size 2 → codes 4=clear, 5=end. Stream: clear (4),
        // index 0 (the black entry), end (5). Encoded with 3-bit codes:
        //   clear = 0b100  (4)
        //   0     = 0b000  (0)
        //   end   = 0b101  (5)
        // Packed LSB-first: bits = 100 000 101 → bytes (LSB first):
        //   byte0 = (4) | (0 << 3) | (5 << 6) = 0b01000100 = 0x44
        //   byte1 = (5 >> 2) = 0b001 = 0x01
        bytes.push(0x02); // LZW min code size
        bytes.push(0x02); // sub-block size
        bytes.extend_from_slice(&[0x44, 0x01]);
        bytes.push(0x00); // sub-block terminator
        // Trailer.
        bytes.push(0x3B);
        bytes
    }

    #[test]
    fn rejects_bad_signature() {
        let r = decode(b"NOTAGIF...");
        assert!(matches!(r, Err(GifError::Malformed(_))));
    }

    #[test]
    fn decodes_single_pixel() {
        let bytes = black_pixel_gif();
        let img = decode(&bytes).expect("decode");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.pixels, vec![0, 0, 0, 255]);
    }

    #[test]
    fn deinterlace_round_trips_a_known_pattern() {
        // 1×8 image: each row gets a unique value. After interlacing
        // these become the order (0,8,…) (4,…) (2,6,…) (1,3,5,7) which
        // for height=8 maps row index pass-order: 0, 4, 2, 6, 1, 3, 5, 7.
        let src: Vec<u8> = vec![0, 4, 2, 6, 1, 3, 5, 7];
        let de = deinterlace(&src, 1, 8);
        assert_eq!(de, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }
}
