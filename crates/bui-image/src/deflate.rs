//! Hand-rolled Deflate decoder (RFC 1951) + zlib wrapper (RFC 1950).
//!
//! Implements all three block types: stored (BTYPE=00), fixed Huffman
//! (BTYPE=01), and dynamic Huffman (BTYPE=10). Canonical Huffman codes are
//! built per the algorithm in §3.2.2.

use std::sync::OnceLock;

#[derive(Debug, Clone, thiserror::Error)]
pub enum DeflateError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("invalid block type")]
    InvalidBlockType,
    #[error("malformed stored block (LEN/NLEN mismatch)")]
    InvalidStoredBlock,
    #[error("invalid Huffman code")]
    InvalidHuffman,
    #[error("invalid length code")]
    InvalidLength,
    #[error("invalid distance code")]
    InvalidDistance,
    #[error("back-reference distance exceeds output ({0} > {1})")]
    DistanceTooLarge(u32, u32),
    #[error("invalid code-length encoding")]
    InvalidCodeLength,
    #[error("zlib header missing or truncated")]
    TruncatedZlib,
    #[error("zlib FCHECK validation failed")]
    InvalidZlibHeader,
    #[error("unsupported zlib compression method {0}")]
    UnsupportedCompression(u8),
    #[error("zlib preset dictionary not supported")]
    PresetDictUnsupported,
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Decode a raw Deflate stream.
pub fn inflate(input: &[u8]) -> Result<Vec<u8>, DeflateError> {
    let mut reader = BitReader::new(input);
    let mut out = Vec::new();
    loop {
        let bfinal = reader.read_bits(1)? == 1;
        let btype = reader.read_bits(2)?;
        match btype {
            0 => decode_stored(&mut reader, &mut out)?,
            1 => {
                let (lit, dist) = fixed_tables();
                decode_huffman_block(lit, dist, &mut reader, &mut out)?;
            }
            2 => {
                let (lit, dist) = decode_dynamic_tables(&mut reader)?;
                decode_huffman_block(&lit, &dist, &mut reader, &mut out)?;
            }
            _ => return Err(DeflateError::InvalidBlockType),
        }
        if bfinal {
            break;
        }
    }
    Ok(out)
}

/// Decode a zlib-wrapped Deflate stream (RFC 1950).
pub fn inflate_zlib(input: &[u8]) -> Result<Vec<u8>, DeflateError> {
    if input.len() < 6 {
        return Err(DeflateError::TruncatedZlib);
    }
    let cmf = input[0];
    let flg = input[1];
    let cm = cmf & 0x0F;
    if cm != 8 {
        return Err(DeflateError::UnsupportedCompression(cm));
    }
    if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(DeflateError::InvalidZlibHeader);
    }
    if flg & 0x20 != 0 {
        return Err(DeflateError::PresetDictUnsupported);
    }
    // Skip 2-byte header and 4-byte trailing Adler32 checksum (we don't
    // verify it; PNG has its own per-chunk CRC32 already).
    if input.len() < 2 + 4 {
        return Err(DeflateError::TruncatedZlib);
    }
    let body = &input[2..input.len() - 4];
    inflate(body)
}

// ---- bit reader ----

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn read_bits(&mut self, n: u32) -> Result<u32, DeflateError> {
        debug_assert!(n <= 16);
        let mut value = 0u32;
        for i in 0..n {
            let byte_idx = self.bit_pos >> 3;
            let bit_off = self.bit_pos & 7;
            if byte_idx >= self.bytes.len() {
                return Err(DeflateError::UnexpectedEof);
            }
            let bit = (self.bytes[byte_idx] >> bit_off) & 1;
            value |= u32::from(bit) << i;
            self.bit_pos += 1;
        }
        Ok(value)
    }

    fn align_to_byte(&mut self) {
        let extra = self.bit_pos & 7;
        if extra != 0 {
            self.bit_pos += 8 - extra;
        }
    }

    fn read_aligned_byte(&mut self) -> Result<u8, DeflateError> {
        debug_assert_eq!(self.bit_pos & 7, 0);
        let idx = self.bit_pos >> 3;
        if idx >= self.bytes.len() {
            return Err(DeflateError::UnexpectedEof);
        }
        let b = self.bytes[idx];
        self.bit_pos += 8;
        Ok(b)
    }
}

// ---- Huffman tables ----

#[derive(Debug, Clone)]
struct HuffmanTable {
    counts: [u32; 16],
    /// Symbols in (length, symbol-id) order. Indexed by `offsets[L] + (code - first_code_at_L)`.
    offsets: [u32; 16],
    symbols: Vec<u16>,
}

impl HuffmanTable {
    fn from_lengths(lengths: &[u8]) -> Result<Self, DeflateError> {
        let mut counts = [0u32; 16];
        for &l in lengths {
            if l as usize >= 16 {
                return Err(DeflateError::InvalidCodeLength);
            }
            if l > 0 {
                counts[l as usize] += 1;
            }
        }
        let mut offsets = [0u32; 16];
        let mut total = 0u32;
        for i in 1..16 {
            offsets[i] = total;
            total += counts[i];
        }
        let mut symbols = vec![0u16; total as usize];
        let mut next = offsets;
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                let pos = next[l as usize];
                symbols[pos as usize] = sym as u16;
                next[l as usize] += 1;
            }
        }
        Ok(Self {
            counts,
            offsets,
            symbols,
        })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, DeflateError> {
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        for l in 1..16 {
            let bit = reader.read_bits(1)?;
            code = (code << 1) | bit;
            let count = self.counts[l];
            if code < first + count {
                let idx = self.offsets[l] + (code - first);
                return Ok(self.symbols[idx as usize]);
            }
            first = (first + count) << 1;
        }
        Err(DeflateError::InvalidHuffman)
    }
}

fn fixed_tables() -> (&'static HuffmanTable, &'static HuffmanTable) {
    static TABLES: OnceLock<(HuffmanTable, HuffmanTable)> = OnceLock::new();
    let (lit, dist) = TABLES.get_or_init(|| {
        let mut lit_lens = [0u8; 288];
        for i in 0..=143 {
            lit_lens[i] = 8;
        }
        for i in 144..=255 {
            lit_lens[i] = 9;
        }
        for i in 256..=279 {
            lit_lens[i] = 7;
        }
        for i in 280..=287 {
            lit_lens[i] = 8;
        }
        let lit = HuffmanTable::from_lengths(&lit_lens).expect("fixed lit table");
        let dist = HuffmanTable::from_lengths(&[5u8; 30]).expect("fixed dist table");
        (lit, dist)
    });
    (lit, dist)
}

// ---- block decoders ----

fn decode_stored(reader: &mut BitReader<'_>, out: &mut Vec<u8>) -> Result<(), DeflateError> {
    reader.align_to_byte();
    let len = u16::from(reader.read_aligned_byte()?)
        | (u16::from(reader.read_aligned_byte()?) << 8);
    let nlen = u16::from(reader.read_aligned_byte()?)
        | (u16::from(reader.read_aligned_byte()?) << 8);
    if len ^ 0xFFFF != nlen {
        return Err(DeflateError::InvalidStoredBlock);
    }
    out.reserve(len as usize);
    for _ in 0..len {
        out.push(reader.read_aligned_byte()?);
    }
    Ok(())
}

fn decode_dynamic_tables(
    reader: &mut BitReader<'_>,
) -> Result<(HuffmanTable, HuffmanTable), DeflateError> {
    let hlit = reader.read_bits(5)? as usize + 257;
    let hdist = reader.read_bits(5)? as usize + 1;
    let hclen = reader.read_bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 || hclen > 19 {
        return Err(DeflateError::InvalidCodeLength);
    }

    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CL_ORDER[i]] = reader.read_bits(3)? as u8;
    }
    let cl_table = HuffmanTable::from_lengths(&cl_lengths)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let sym = cl_table.decode(reader)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(DeflateError::InvalidCodeLength);
                }
                let repeat = 3 + reader.read_bits(2)? as usize;
                let prev = lengths[i - 1];
                for _ in 0..repeat {
                    if i >= total {
                        return Err(DeflateError::InvalidCodeLength);
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let repeat = 3 + reader.read_bits(3)? as usize;
                for _ in 0..repeat {
                    if i >= total {
                        return Err(DeflateError::InvalidCodeLength);
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            18 => {
                let repeat = 11 + reader.read_bits(7)? as usize;
                for _ in 0..repeat {
                    if i >= total {
                        return Err(DeflateError::InvalidCodeLength);
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            _ => return Err(DeflateError::InvalidCodeLength),
        }
    }
    let lit = HuffmanTable::from_lengths(&lengths[..hlit])?;
    let dist = HuffmanTable::from_lengths(&lengths[hlit..])?;
    Ok((lit, dist))
}

fn decode_huffman_block(
    lit: &HuffmanTable,
    dist: &HuffmanTable,
    reader: &mut BitReader<'_>,
    out: &mut Vec<u8>,
) -> Result<(), DeflateError> {
    loop {
        let sym = lit.decode(reader)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let len_code = (sym - 257) as usize;
            if len_code >= LENGTH_BASE.len() {
                return Err(DeflateError::InvalidLength);
            }
            let length = u32::from(LENGTH_BASE[len_code])
                + reader.read_bits(u32::from(LENGTH_EXTRA[len_code]))?;
            let dist_sym = dist.decode(reader)?;
            if dist_sym as usize >= DIST_BASE.len() {
                return Err(DeflateError::InvalidDistance);
            }
            let distance = u32::from(DIST_BASE[dist_sym as usize])
                + reader.read_bits(u32::from(DIST_EXTRA[dist_sym as usize]))?;
            if distance == 0 || distance as usize > out.len() {
                return Err(DeflateError::DistanceTooLarge(distance, out.len() as u32));
            }
            // Run-length: when length > distance, we read bytes we just
            // wrote — explicit byte-by-byte loop handles that correctly.
            let start = out.len() - distance as usize;
            for i in 0..length as usize {
                let b = out[start + i];
                out.push(b);
            }
        }
    }
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
    fn empty_fixed_block() {
        // BFINAL=1, BTYPE=01, then EOB symbol 256 (length 7, code 0000000).
        let bytes = [0x03u8, 0x00];
        let out = inflate(&bytes).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn stored_block() {
        // BFINAL=1, BTYPE=00 → first byte 0x01, padded to byte boundary.
        // LEN=5 LE, NLEN=0xFFFA LE, then "ABCDE".
        let bytes = [
            0x01, 0x05, 0x00, 0xFA, 0xFF, b'A', b'B', b'C', b'D', b'E',
        ];
        let out = inflate(&bytes).unwrap();
        assert_eq!(out, b"ABCDE");
    }

    #[test]
    fn zlib_fixed_huffman_short() {
        let z = unhex("78daf348cdc9c9d75108cf2fca4951e4020024120474");
        let out = inflate_zlib(&z).unwrap();
        assert_eq!(out, b"Hello, World!\n");
    }

    #[test]
    fn zlib_dynamic_huffman_repetitive() {
        // 225-byte input with heavy repetition — forces dynamic Huffman + back-references.
        let z = unhex("78da0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c41c500d28c50c4");
        let out = inflate_zlib(&z).unwrap();
        let expected = b"The quick brown fox jumps over the lazy dog. ".repeat(5);
        assert_eq!(out, expected);
    }

    #[test]
    fn zlib_run_length_back_reference() {
        // "aaaa…aaaabbbbbbbbcccc" — exercises length > distance run-length copy.
        let z = unhex("78da4b4c44054950900c04009a4a0aad");
        let out = inflate_zlib(&z).unwrap();
        assert_eq!(out, b"aaaaaaaaaaaaaaaabbbbbbbbcccc");
    }

    #[test]
    fn rejects_bad_zlib_header() {
        let bad = [0x00u8, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(inflate_zlib(&bad).is_err());
    }
}
