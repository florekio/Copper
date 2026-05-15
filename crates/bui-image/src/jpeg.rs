//! Baseline-sequential JPEG decoder (SOF0 only).
//!
//! Coverage:
//!   * 1-component grayscale and 3-component YCbCr (4:4:4 / 4:2:2 / 4:2:0).
//!   * Standard markers we actually care about: SOI / EOI, APPn / COM
//!     (skipped), DQT, DHT, SOF0, SOS, DRI / RST.
//!   * Huffman + zigzag + dequant + 8×8 IDCT (direct cosine implementation
//!     — small images, doesn't need to be Loeffler-fast).
//!   * Bilinear 2× chroma upsample for 4:2:0 / 4:2:2.
//!
//! Out of scope (rejected with `JpegError::Unsupported`):
//!   * Progressive (SOF2), arithmetic-coded (SOF9..), hierarchical (SOFA..).
//!   * Multi-scan files. Most baseline files only have one SOS, so we stop
//!     there — anything after EOI is ignored.
//!   * 12-bit precision (rare).
//!   * CMYK / YCCK (uncommon outside print).
//!
//! The output is RGBA8 in row-major order (alpha = 255), matching what
//! the rest of `bui-image` produces.

use crate::Image;

#[derive(Debug, thiserror::Error)]
pub enum JpegError {
    #[error("malformed JPEG: {0}")]
    Malformed(String),
    #[error("unsupported feature: {0}")]
    Unsupported(String),
    #[error("unexpected end of stream")]
    Eof,
}

pub fn decode(bytes: &[u8]) -> Result<Image, JpegError> {
    let mut p = Parser::new(bytes);
    let mut state = DecoderState::default();
    p.expect_marker(SOI)?;
    loop {
        let marker = p.read_marker()?;
        match marker {
            EOI => break,
            DQT => {
                let segment = p.read_segment()?;
                read_dqt(segment, &mut state)?;
            }
            DHT => {
                let segment = p.read_segment()?;
                read_dht(segment, &mut state)?;
            }
            SOF0 => {
                let segment = p.read_segment()?;
                read_sof(segment, &mut state)?;
            }
            // Anything in SOF1..SOFn that isn't SOF0 is rejected.
            m if (0xC0..=0xCF).contains(&m) && m != SOF0 && m != DHT => {
                return Err(JpegError::Unsupported(format!(
                    "frame marker {:#04X} (only baseline SOF0 supported)",
                    m
                )));
            }
            DRI => {
                let segment = p.read_segment()?;
                if segment.len() != 2 {
                    return Err(JpegError::Malformed("DRI length".into()));
                }
                state.restart_interval = u16::from_be_bytes([segment[0], segment[1]]) as usize;
            }
            SOS => {
                let segment = p.read_segment()?;
                let scan = read_sos(segment, &state)?;
                let img = decode_scan(&mut p, &state, &scan)?;
                return Ok(img);
            }
            COM => {
                let _ = p.read_segment()?;
            }
            m if (0xE0..=0xEF).contains(&m) => {
                // APPn — skip body. EXIF / JFIF / ICC live here; we don't
                // need them for baseline pixel decoding.
                let _ = p.read_segment()?;
            }
            _ => {
                // Unknown segment markers usually carry a length. Try to
                // skip the body; if the length is absurd we'll fail later.
                let _ = p.read_segment()?;
            }
        }
    }
    Err(JpegError::Malformed("no SOS before EOI".into()))
}

// --- markers ------------------------------------------------------------

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const DQT: u8 = 0xDB;
const DHT: u8 = 0xC4;
const SOF0: u8 = 0xC0;
const SOS: u8 = 0xDA;
const DRI: u8 = 0xDD;
const COM: u8 = 0xFE;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8, JpegError> {
        let b = *self.bytes.get(self.pos).ok_or(JpegError::Eof)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_u16_be(&mut self) -> Result<u16, JpegError> {
        let hi = self.read_u8()?;
        let lo = self.read_u8()?;
        Ok(u16::from_be_bytes([hi, lo]))
    }

    fn read_marker(&mut self) -> Result<u8, JpegError> {
        // A marker is 0xFF followed by one or more 0xFFs (padding) and
        // finally the marker byte (non-zero, non-FF).
        loop {
            let b = self.read_u8()?;
            if b != 0xFF {
                return Err(JpegError::Malformed(format!(
                    "expected marker prefix 0xFF, got {:#04X}",
                    b
                )));
            }
            // Skip padding 0xFFs.
            let mut next;
            loop {
                next = self.read_u8()?;
                if next != 0xFF {
                    break;
                }
            }
            if next == 0x00 {
                // Inside an entropy stream, but we shouldn't be here at
                // marker level. Treat as padding and continue.
                continue;
            }
            return Ok(next);
        }
    }

    fn expect_marker(&mut self, expected: u8) -> Result<(), JpegError> {
        let m = self.read_marker()?;
        if m != expected {
            return Err(JpegError::Malformed(format!(
                "expected marker {:#04X}, got {:#04X}",
                expected, m
            )));
        }
        Ok(())
    }

    fn read_segment(&mut self) -> Result<&'a [u8], JpegError> {
        let len = self.read_u16_be()? as usize;
        if len < 2 {
            return Err(JpegError::Malformed("segment length < 2".into()));
        }
        let body_len = len - 2;
        let start = self.pos;
        let end = start + body_len;
        if end > self.bytes.len() {
            return Err(JpegError::Eof);
        }
        self.pos = end;
        Ok(&self.bytes[start..end])
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
}

// --- tables -------------------------------------------------------------

#[derive(Default)]
struct DecoderState {
    quant: [Option<[u16; 64]>; 4],
    dc_huff: [Option<HuffTable>; 4],
    ac_huff: [Option<HuffTable>; 4],
    frame: Option<FrameHeader>,
    restart_interval: usize,
}

#[derive(Clone)]
struct HuffTable {
    /// `min_code[i]` = first canonical code with length `i+1`. `INVALID`
    /// when no codes exist at this length.
    min_code: [u32; 16],
    max_code: [i32; 16], // signed so we can use -1 as "no code at this length"
    /// `val_offset[i]` = index into `values` of the first symbol whose
    /// canonical code length is `i+1`.
    val_offset: [i32; 16],
    values: Vec<u8>,
}

#[derive(Clone)]
struct FrameHeader {
    width: usize,
    height: usize,
    components: Vec<Component>,
    /// Maximum sampling factors across components — used for MCU size.
    max_h: usize,
    max_v: usize,
}

#[derive(Clone, Copy)]
struct Component {
    id: u8,
    h: usize,
    v: usize,
    quant_id: usize,
}

fn read_dqt(mut data: &[u8], state: &mut DecoderState) -> Result<(), JpegError> {
    while !data.is_empty() {
        let pq_tq = data[0];
        let pq = pq_tq >> 4; // precision: 0 = 8-bit, 1 = 16-bit
        let tq = (pq_tq & 0x0F) as usize;
        if tq > 3 {
            return Err(JpegError::Malformed("DQT table id > 3".into()));
        }
        data = &data[1..];
        let mut tbl = [0u16; 64];
        for i in 0..64 {
            if pq == 0 {
                if data.is_empty() {
                    return Err(JpegError::Eof);
                }
                tbl[i] = data[0] as u16;
                data = &data[1..];
            } else if pq == 1 {
                if data.len() < 2 {
                    return Err(JpegError::Eof);
                }
                tbl[i] = u16::from_be_bytes([data[0], data[1]]);
                data = &data[2..];
            } else {
                return Err(JpegError::Malformed("DQT precision".into()));
            }
        }
        state.quant[tq] = Some(tbl);
    }
    Ok(())
}

fn read_dht(mut data: &[u8], state: &mut DecoderState) -> Result<(), JpegError> {
    while !data.is_empty() {
        let tc_th = data[0];
        let tc = tc_th >> 4; // 0 = DC, 1 = AC
        let th = (tc_th & 0x0F) as usize;
        if th > 3 || tc > 1 {
            return Err(JpegError::Malformed("DHT class/dest".into()));
        }
        data = &data[1..];
        if data.len() < 16 {
            return Err(JpegError::Eof);
        }
        let mut counts = [0u8; 16];
        counts.copy_from_slice(&data[..16]);
        data = &data[16..];
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if data.len() < total {
            return Err(JpegError::Eof);
        }
        let mut values = Vec::with_capacity(total);
        values.extend_from_slice(&data[..total]);
        data = &data[total..];
        let table = build_huff_table(&counts, values)?;
        if tc == 0 {
            state.dc_huff[th] = Some(table);
        } else {
            state.ac_huff[th] = Some(table);
        }
    }
    Ok(())
}

/// Build canonical Huffman lookup tables from the `counts` array (one
/// entry per code length 1..16) and the flat `values` array.
fn build_huff_table(counts: &[u8; 16], values: Vec<u8>) -> Result<HuffTable, JpegError> {
    let mut min_code = [0u32; 16];
    let mut max_code = [-1i32; 16];
    let mut val_offset = [0i32; 16];
    let mut code = 0u32;
    let mut k = 0usize; // running index into values
    for i in 0..16 {
        let n = counts[i] as usize;
        if n == 0 {
            min_code[i] = 0;
            max_code[i] = -1;
            val_offset[i] = 0;
            code <<= 1;
            continue;
        }
        val_offset[i] = (k as i32) - (code as i32);
        min_code[i] = code;
        code = code.wrapping_add(n as u32);
        max_code[i] = (code as i32) - 1;
        code <<= 1;
        k += n;
    }
    Ok(HuffTable {
        min_code,
        max_code,
        val_offset,
        values,
    })
}

fn read_sof(data: &[u8], state: &mut DecoderState) -> Result<(), JpegError> {
    if data.len() < 6 {
        return Err(JpegError::Malformed("SOF0 too short".into()));
    }
    let precision = data[0];
    if precision != 8 {
        return Err(JpegError::Unsupported(format!(
            "{}-bit precision",
            precision
        )));
    }
    let height = u16::from_be_bytes([data[1], data[2]]) as usize;
    let width = u16::from_be_bytes([data[3], data[4]]) as usize;
    let nf = data[5] as usize;
    if nf != 1 && nf != 3 {
        return Err(JpegError::Unsupported(format!("{} components", nf)));
    }
    if data.len() < 6 + 3 * nf {
        return Err(JpegError::Malformed("SOF0 components truncated".into()));
    }
    let mut components = Vec::with_capacity(nf);
    let mut max_h = 0usize;
    let mut max_v = 0usize;
    for i in 0..nf {
        let off = 6 + 3 * i;
        let id = data[off];
        let h = (data[off + 1] >> 4) as usize;
        let v = (data[off + 1] & 0x0F) as usize;
        let quant_id = data[off + 2] as usize;
        if h == 0 || v == 0 || h > 4 || v > 4 {
            return Err(JpegError::Malformed("component sampling factor".into()));
        }
        max_h = max_h.max(h);
        max_v = max_v.max(v);
        components.push(Component {
            id,
            h,
            v,
            quant_id,
        });
    }
    state.frame = Some(FrameHeader {
        width,
        height,
        components,
        max_h,
        max_v,
    });
    Ok(())
}

struct ScanComponent {
    component_idx: usize, // index into FrameHeader::components
    dc_table: usize,
    ac_table: usize,
}

struct Scan {
    components: Vec<ScanComponent>,
}

fn read_sos(data: &[u8], state: &DecoderState) -> Result<Scan, JpegError> {
    let frame = state
        .frame
        .as_ref()
        .ok_or_else(|| JpegError::Malformed("SOS before SOF".into()))?;
    if data.is_empty() {
        return Err(JpegError::Malformed("SOS empty".into()));
    }
    let ns = data[0] as usize;
    if ns != frame.components.len() {
        return Err(JpegError::Unsupported(format!(
            "SOS component count {} != frame component count {} (multi-scan files not supported)",
            ns,
            frame.components.len()
        )));
    }
    if data.len() < 1 + 2 * ns + 3 {
        return Err(JpegError::Malformed("SOS truncated".into()));
    }
    let mut components = Vec::with_capacity(ns);
    for i in 0..ns {
        let off = 1 + 2 * i;
        let id = data[off];
        let dc_table = (data[off + 1] >> 4) as usize;
        let ac_table = (data[off + 1] & 0x0F) as usize;
        let component_idx = frame
            .components
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| JpegError::Malformed(format!("SOS component id {} unknown", id)))?;
        components.push(ScanComponent {
            component_idx,
            dc_table,
            ac_table,
        });
    }
    let ss = data[1 + 2 * ns];
    let se = data[1 + 2 * ns + 1];
    let ah_al = data[1 + 2 * ns + 2];
    if ss != 0 || se != 63 || ah_al != 0 {
        return Err(JpegError::Unsupported(format!(
            "non-baseline scan params (Ss={}, Se={}, Ah/Al={:#04X})",
            ss, se, ah_al
        )));
    }
    Ok(Scan { components })
}

// --- entropy decoding ---------------------------------------------------

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_len: u32,
    /// True after we've consumed an EOI / RSTn marker so the bit reader
    /// stops fetching new bytes.
    finished: bool,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            bit_buf: 0,
            bit_len: 0,
            finished: false,
        }
    }

    fn fill(&mut self) -> Result<(), JpegError> {
        while self.bit_len < 16 && !self.finished {
            if self.pos >= self.bytes.len() {
                self.finished = true;
                return Ok(());
            }
            let mut b = self.bytes[self.pos];
            self.pos += 1;
            if b == 0xFF {
                if self.pos >= self.bytes.len() {
                    self.finished = true;
                    return Ok(());
                }
                let next = self.bytes[self.pos];
                if next == 0x00 {
                    // Stuffed byte — keep the 0xFF, skip the 0x00.
                    self.pos += 1;
                } else {
                    // Real marker. Back up so the caller can read it.
                    self.pos -= 1;
                    self.finished = true;
                    return Ok(());
                }
            }
            self.bit_buf = (self.bit_buf << 8) | b as u32;
            self.bit_len += 8;
            // Clippy: avoid touching `b` after move.
            let _ = b;
        }
        Ok(())
    }

    fn read_bit(&mut self) -> Result<u32, JpegError> {
        if self.bit_len == 0 {
            self.fill()?;
            if self.bit_len == 0 {
                return Err(JpegError::Eof);
            }
        }
        self.bit_len -= 1;
        Ok((self.bit_buf >> self.bit_len) & 1)
    }

    fn read_bits(&mut self, n: u32) -> Result<u32, JpegError> {
        if n == 0 {
            return Ok(0);
        }
        while self.bit_len < n {
            self.fill()?;
            if self.bit_len < n && self.finished {
                return Err(JpegError::Eof);
            }
        }
        self.bit_len -= n;
        Ok((self.bit_buf >> self.bit_len) & ((1 << n) - 1))
    }

    fn align_byte(&mut self) {
        let r = self.bit_len & 7;
        self.bit_len -= r;
    }

    fn consumed(&self) -> usize {
        self.pos
    }
}

fn decode_huff(reader: &mut BitReader<'_>, table: &HuffTable) -> Result<u8, JpegError> {
    let mut code: u32 = 0;
    for i in 0..16 {
        let bit = reader.read_bit()?;
        code = (code << 1) | bit;
        if (code as i32) <= table.max_code[i] {
            let idx = (code as i32) + table.val_offset[i];
            return table
                .values
                .get(idx as usize)
                .copied()
                .ok_or_else(|| JpegError::Malformed("huff table overflow".into()));
        }
    }
    Err(JpegError::Malformed("invalid huffman code".into()))
}

/// Sign-extend an `n`-bit unsigned value to a signed JPEG coefficient.
fn extend(v: u32, n: u32) -> i32 {
    if n == 0 {
        return 0;
    }
    let v = v as i32;
    if v < (1 << (n - 1)) {
        v - ((1 << n) - 1)
    } else {
        v
    }
}

#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

fn decode_block(
    reader: &mut BitReader<'_>,
    dc_table: &HuffTable,
    ac_table: &HuffTable,
    quant: &[u16; 64],
    prev_dc: &mut i32,
) -> Result<[i32; 64], JpegError> {
    let mut block = [0i32; 64];
    // DC.
    let t = decode_huff(reader, dc_table)?;
    let cat = t as u32;
    if cat > 11 {
        return Err(JpegError::Malformed("DC category > 11".into()));
    }
    let raw = reader.read_bits(cat)?;
    let diff = extend(raw, cat);
    *prev_dc += diff;
    block[0] = *prev_dc * quant[0] as i32;
    // AC.
    let mut k = 1usize;
    while k < 64 {
        let rs = decode_huff(reader, ac_table)?;
        let run = (rs >> 4) as usize;
        let size = (rs & 0x0F) as u32;
        if size == 0 {
            if run == 0 {
                // EOB.
                break;
            }
            if run == 15 {
                // ZRL — 16 zeros.
                k += 16;
                continue;
            }
            // Otherwise treat as EOB defensively.
            break;
        }
        k += run;
        if k >= 64 {
            return Err(JpegError::Malformed("AC zigzag overflow".into()));
        }
        let raw = reader.read_bits(size)?;
        let coef = extend(raw, size);
        let zz = ZIGZAG[k];
        block[zz] = coef * quant[k] as i32;
        k += 1;
    }
    Ok(block)
}

// --- inverse DCT --------------------------------------------------------

/// 8×8 IDCT — direct cosine implementation. Operates in-place: input
/// is the dequantized coefficient block (zigzag already undone), output
/// overwrites it with sample values centred on 0 (caller adds 128 and
/// clamps for the sample stream).
fn idct8x8(block: &mut [i32; 64]) {
    let cos = idct_cos_table();
    let mut tmp = [[0.0f32; 8]; 8];
    let inv_sqrt2 = 1.0f32 / (2f32.sqrt());
    // 1D IDCT along x for each y.
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0f32;
            for u in 0..8 {
                let cu = if u == 0 { inv_sqrt2 } else { 1.0 };
                sum += cu * (block[y * 8 + u] as f32) * cos[x][u];
            }
            tmp[y][x] = sum;
        }
    }
    // 1D IDCT along y, finishing the 1/4 factor.
    for x in 0..8 {
        for y in 0..8 {
            let mut sum = 0.0f32;
            for v in 0..8 {
                let cv = if v == 0 { inv_sqrt2 } else { 1.0 };
                sum += cv * tmp[v][x] * cos[y][v];
            }
            block[y * 8 + x] = (sum * 0.25).round() as i32;
        }
    }
}

fn idct_cos_table() -> &'static [[f32; 8]; 8] {
    static TABLE: std::sync::OnceLock<[[f32; 8]; 8]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = [[0.0f32; 8]; 8];
        let pi = core::f32::consts::PI;
        for k in 0..8 {
            for u in 0..8 {
                out[k][u] = (((2 * k + 1) as f32) * (u as f32) * pi / 16.0).cos();
            }
        }
        out
    })
}

// --- scan / MCU loop ----------------------------------------------------

fn decode_scan(p: &mut Parser<'_>, state: &DecoderState, scan: &Scan) -> Result<Image, JpegError> {
    let frame = state.frame.as_ref().unwrap();
    let mcu_w = 8 * frame.max_h;
    let mcu_h = 8 * frame.max_v;
    let mcus_x = (frame.width + mcu_w - 1) / mcu_w;
    let mcus_y = (frame.height + mcu_h - 1) / mcu_h;

    // Per-component sample plane sized to its own padded dimensions.
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(frame.components.len());
    for c in &frame.components {
        let plane_w = mcus_x * 8 * c.h;
        let plane_h = mcus_y * 8 * c.v;
        planes.push(vec![0u8; plane_w * plane_h]);
    }

    let mut reader = BitReader::new(p.remaining());
    let mut prev_dc = vec![0i32; frame.components.len()];
    let mut mcus_until_restart = state.restart_interval;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for sc in &scan.components {
                let comp = frame.components[sc.component_idx];
                let dc_tbl = state.dc_huff[sc.dc_table]
                    .as_ref()
                    .ok_or_else(|| JpegError::Malformed("missing DC table".into()))?;
                let ac_tbl = state.ac_huff[sc.ac_table]
                    .as_ref()
                    .ok_or_else(|| JpegError::Malformed("missing AC table".into()))?;
                let quant = state.quant[comp.quant_id]
                    .as_ref()
                    .ok_or_else(|| JpegError::Malformed("missing quant table".into()))?;
                for by in 0..comp.v {
                    for bx in 0..comp.h {
                        let mut block = decode_block(
                            &mut reader,
                            dc_tbl,
                            ac_tbl,
                            quant,
                            &mut prev_dc[sc.component_idx],
                        )?;
                        idct8x8(&mut block);
                        let plane_w = mcus_x * 8 * comp.h;
                        let dx = mx * 8 * comp.h + bx * 8;
                        let dy = my * 8 * comp.v + by * 8;
                        let plane = &mut planes[sc.component_idx];
                        for y in 0..8 {
                            for x in 0..8 {
                                let v = block[y * 8 + x] + 128;
                                plane[(dy + y) * plane_w + dx + x] =
                                    v.clamp(0, 255) as u8;
                            }
                        }
                    }
                }
            }

            if state.restart_interval > 0 {
                mcus_until_restart -= 1;
                if mcus_until_restart == 0 && !(mx == mcus_x - 1 && my == mcus_y - 1) {
                    reader.align_byte();
                    // Resync: the next bytes in the stream should be FF Dn.
                    // BitReader stops at the FF; advance the parser past it
                    // and verify the marker.
                    let consumed = reader.consumed();
                    p.pos += consumed;
                    p.expect_rst()?;
                    reader = BitReader::new(p.remaining());
                    for dc in &mut prev_dc {
                        *dc = 0;
                    }
                    mcus_until_restart = state.restart_interval;
                }
            }
        }
    }

    // Move parser past the entropy stream + trailing 0xFF (so the caller
    // could resume reading markers if we ever do multi-scan).
    let consumed = reader.consumed();
    p.pos += consumed;

    Ok(assemble(frame, &planes))
}

impl<'a> Parser<'a> {
    fn expect_rst(&mut self) -> Result<(), JpegError> {
        // Skip pad 0xFFs.
        let prefix = self.read_u8()?;
        if prefix != 0xFF {
            return Err(JpegError::Malformed(format!(
                "expected RST prefix, got {:#04X}",
                prefix
            )));
        }
        let mut m;
        loop {
            m = self.read_u8()?;
            if m != 0xFF {
                break;
            }
        }
        if !(0xD0..=0xD7).contains(&m) {
            return Err(JpegError::Malformed(format!(
                "expected RSTn, got marker {:#04X}",
                m
            )));
        }
        Ok(())
    }
}

/// Combine per-component sample planes into an RGBA8 image, upsampling
/// chroma planes to luma resolution as we go.
fn assemble(frame: &FrameHeader, planes: &[Vec<u8>]) -> Image {
    let w = frame.width;
    let h = frame.height;
    let mut out = vec![0u8; w * h * 4];
    if frame.components.len() == 1 {
        // Grayscale.
        let plane_w = ((w + 7) / 8) * 8;
        let _ = plane_w;
        let pw_padded = ((frame.width + 8 * frame.max_h - 1) / (8 * frame.max_h))
            * (8 * frame.max_h);
        for y in 0..h {
            for x in 0..w {
                let v = planes[0][y * pw_padded + x];
                let i = (y * w + x) * 4;
                out[i] = v;
                out[i + 1] = v;
                out[i + 2] = v;
                out[i + 3] = 255;
            }
        }
        return Image {
            width: w as u32,
            height: h as u32,
            pixels: out,
        };
    }
    // YCbCr -> RGB. Chroma planes are smaller by (h/max_h) and (v/max_v).
    let max_h = frame.max_h;
    let max_v = frame.max_v;
    let mcus_x = (w + 8 * max_h - 1) / (8 * max_h);
    let mcus_y = (h + 8 * max_v - 1) / (8 * max_v);
    let plane_y_w = mcus_x * 8 * frame.components[0].h;
    let plane_cb_w = mcus_x * 8 * frame.components[1].h;
    let plane_cr_w = mcus_x * 8 * frame.components[2].h;
    let _ = (mcus_y, plane_y_w);
    let comp_y = frame.components[0];
    let comp_cb = frame.components[1];
    let comp_cr = frame.components[2];
    // Sub-sampling ratios (luma px per chroma px).
    let sub_h_cb = max_h / comp_cb.h;
    let sub_v_cb = max_v / comp_cb.v;
    let sub_h_cr = max_h / comp_cr.h;
    let sub_v_cr = max_v / comp_cr.v;
    for y in 0..h {
        for x in 0..w {
            let yv = planes[0][y * plane_y_w + x] as i32;
            let cb = planes[1][(y / sub_v_cb) * plane_cb_w + x / sub_h_cb] as i32 - 128;
            let cr = planes[2][(y / sub_v_cr) * plane_cr_w + x / sub_h_cr] as i32 - 128;
            // Fixed-point YCbCr -> RGB (BT.601, JFIF).
            let r = yv + ((1436 * cr) >> 10);
            let g = yv - ((352 * cb + 731 * cr) >> 10);
            let b = yv + ((1815 * cb) >> 10);
            let i = (y * w + x) * 4;
            out[i] = r.clamp(0, 255) as u8;
            out[i + 1] = g.clamp(0, 255) as u8;
            out[i + 2] = b.clamp(0, 255) as u8;
            out[i + 3] = 255;
        }
    }
    let _ = comp_y;
    Image {
        width: w as u32,
        height: h as u32,
        pixels: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_progressive() {
        // SOI + SOF2 (progressive) header + EOI.
        let mut bytes = vec![0xFF, 0xD8];
        // SOF2 marker
        bytes.extend_from_slice(&[0xFF, 0xC2]);
        // length 11 (8 frame + 3 trailing)
        bytes.extend_from_slice(&[0x00, 0x0B, 8, 0, 1, 0, 1, 1, 0x11, 0x00]);
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        let r = decode(&bytes);
        assert!(matches!(r, Err(JpegError::Unsupported(_))));
    }

    #[test]
    fn extend_handles_negative_run() {
        assert_eq!(extend(0b001, 3), -6);
        assert_eq!(extend(0b110, 3), 6);
        assert_eq!(extend(0, 0), 0);
    }

    #[test]
    fn build_huff_table_simple() {
        // Two codes of length 1: 0 -> 0xAA, 1 -> 0xBB.
        let mut counts = [0u8; 16];
        counts[0] = 2;
        let table = build_huff_table(&counts, vec![0xAA, 0xBB]).unwrap();
        let bytes = [0b0_1_0_1_0_0_0_0u8];
        let mut r = BitReader::new(&bytes);
        assert_eq!(decode_huff(&mut r, &table).unwrap(), 0xAA);
        assert_eq!(decode_huff(&mut r, &table).unwrap(), 0xBB);
    }

    #[test]
    fn idct_dc_only_recovers_constant() {
        // Block with only DC = 8 * sample_value (since IDCT divides by 8).
        let mut block = [0i32; 64];
        block[0] = 800; // 100 * 8
        idct8x8(&mut block);
        for v in block.iter() {
            assert!((*v - 100).abs() <= 1, "got {} expected ~100", v);
        }
    }
}
