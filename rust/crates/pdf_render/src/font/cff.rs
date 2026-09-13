//! CFF (Compact Font Format) outlines — `/FontFile3`, and the `CFF ` table of
//! an OpenType program.
//!
//! This is the other half of PDF text. TrueType covers `/FontFile2`, but a
//! large share of real documents — anything produced by a PostScript-lineage
//! pipeline, which includes most invoices, tickets and statements — embeds
//! Type1C or CIDFontType0C instead. Without an interpreter for it those pages
//! draw their graphics and none of their words.
//!
//! Two formats live here: the CFF container (INDEX and DICT structures, the
//! charset, and for CID-keyed fonts the FDArray/FDSelect split), and the Type 2
//! charstring language whose programs actually describe the outlines.

use std::collections::HashMap;

use crate::geom::Path;

/// A parsed CFF font program.
pub struct CffFont {
    data: Vec<u8>,
    /// Byte ranges of each glyph's charstring, indexed by glyph id.
    charstrings: Vec<(usize, usize)>,
    global_subrs: Vec<(usize, usize)>,
    /// Local subroutines for a non-CID font.
    local_subrs: Vec<(usize, usize)>,
    /// Per-FD local subroutines for a CID-keyed font.
    fd_local_subrs: Vec<Vec<(usize, usize)>>,
    /// Glyph id → FD index, for CID-keyed fonts.
    fd_select: Vec<u8>,
    /// Glyph id → SID (plain fonts) or CID (CID-keyed fonts).
    charset: Vec<u16>,
    /// CID → glyph id, the inverse of [charset] for CID-keyed fonts.
    cid_to_gid: HashMap<u16, u16>,
    /// Glyph name → glyph id, for simple fonts addressed by name.
    name_to_gid: HashMap<String, u16>,
    is_cid: bool,
    /// Design units per em, derived from `/FontMatrix` (1000 for most fonts).
    pub units_per_em: f64,
}

impl CffFont {
    /// Parse a bare CFF program, or an OpenType/OTTO wrapper around one.
    pub fn parse(data: Vec<u8>) -> Option<CffFont> {
        let cff = if is_sfnt(&data) {
            extract_cff_table(&data)?
        } else {
            data
        };
        Self::parse_bare(cff)
    }

    fn parse_bare(data: Vec<u8>) -> Option<CffFont> {
        // Header: major, minor, hdrSize, offSize.
        let hdr_size = *data.get(2)? as usize;
        let mut pos = hdr_size;

        let (_names, next) = read_index(&data, pos)?;
        pos = next;
        let (top_dicts, next) = read_index(&data, pos)?;
        pos = next;
        let (_strings, next) = read_index(&data, pos)?;
        pos = next;
        let (global_subrs, _) = read_index(&data, pos)?;

        let top_range = *top_dicts.first()?;
        let top = parse_dict(&data[top_range.0..top_range.1]);

        let font_matrix = top
            .get(&Operator::Escape(7))
            .filter(|v| v.len() >= 4)
            .map(|v| v[0])
            .filter(|scale| *scale > 0.0)
            .unwrap_or(0.001);
        let units_per_em = (1.0 / font_matrix).clamp(1.0, 16384.0);

        let charstrings_offset = *top.get(&Operator::Plain(17))?.first()? as usize;
        let (charstrings, _) = read_index(&data, charstrings_offset)?;
        let num_glyphs = charstrings.len();
        if num_glyphs == 0 {
            return None;
        }

        let is_cid = top.contains_key(&Operator::Escape(30));

        // Private DICT: [size, offset]. Its Subrs offset is relative to it.
        let local_subrs = top
            .get(&Operator::Plain(18))
            .filter(|v| v.len() >= 2)
            .map(|v| (v[1] as usize, v[0] as usize))
            .and_then(|(offset, size)| read_private_subrs(&data, offset, size))
            .unwrap_or_default();

        // CID-keyed fonts split the private data across an FDArray, selected
        // per glyph by FDSelect. A glyph interpreted with the wrong local
        // subroutines produces confident nonsense, so both are needed.
        let mut fd_local_subrs = Vec::new();
        if let Some(fd_array_offset) = top.get(&Operator::Escape(36)).and_then(|v| v.first()) {
            if let Some((fd_dicts, _)) = read_index(&data, *fd_array_offset as usize) {
                for range in fd_dicts {
                    let dict = parse_dict(&data[range.0..range.1]);
                    let subrs = dict
                        .get(&Operator::Plain(18))
                        .filter(|v| v.len() >= 2)
                        .map(|v| (v[1] as usize, v[0] as usize))
                        .and_then(|(offset, size)| read_private_subrs(&data, offset, size))
                        .unwrap_or_default();
                    fd_local_subrs.push(subrs);
                }
            }
        }
        let fd_select = top
            .get(&Operator::Escape(37))
            .and_then(|v| v.first())
            .map(|offset| read_fd_select(&data, *offset as usize, num_glyphs))
            .unwrap_or_default();

        let charset_offset = top
            .get(&Operator::Plain(15))
            .and_then(|v| v.first())
            .copied()
            .unwrap_or(0.0) as usize;
        let charset = read_charset(&data, charset_offset, num_glyphs);

        let mut cid_to_gid = HashMap::new();
        let mut name_to_gid = HashMap::new();
        for (gid, &sid) in charset.iter().enumerate() {
            if is_cid {
                cid_to_gid.insert(sid, gid as u16);
            } else if let Some(name) = sid_name(sid, &_strings, &data) {
                name_to_gid.insert(name, gid as u16);
            }
        }

        Some(CffFont {
            data,
            charstrings,
            global_subrs,
            local_subrs,
            fd_local_subrs,
            fd_select,
            charset,
            cid_to_gid,
            name_to_gid,
            is_cid,
            units_per_em,
        })
    }

    pub fn num_glyphs(&self) -> usize {
        self.charstrings.len()
    }

    pub fn is_cid_keyed(&self) -> bool {
        self.is_cid
    }

    pub fn gid_for_name(&self, name: &str) -> Option<u16> {
        self.name_to_gid.get(name).copied()
    }

    /// For a CID-keyed font, map a CID through the charset. Plain CFF programs
    /// used as a Type0 descendant are already indexed by glyph id.
    pub fn gid_for_cid(&self, cid: u16) -> Option<u16> {
        if !self.is_cid {
            return (usize::from(cid) < self.charstrings.len()).then_some(cid);
        }
        self.cid_to_gid.get(&cid).copied()
    }

    /// Outline for a glyph, in font units with the y axis up.
    pub fn glyph_outline(&self, gid: u16) -> Option<Path> {
        let range = *self.charstrings.get(usize::from(gid))?;
        let local = self.local_subrs_for(gid);
        let mut ctx = CharStringCtx {
            font: self,
            local,
            path: Path::with_tolerance(self.units_per_em / 300.0),
            stack: Vec::with_capacity(48),
            x: 0.0,
            y: 0.0,
            stems: 0,
            width_parsed: false,
            open: false,
            depth: 0,
            trans: Vec::new(),
        };
        ctx.run(range);
        ctx.finish()
    }

    fn local_subrs_for(&self, gid: u16) -> &[(usize, usize)] {
        if self.fd_local_subrs.is_empty() {
            return &self.local_subrs;
        }
        let fd = self
            .fd_select
            .get(usize::from(gid))
            .copied()
            .unwrap_or(0) as usize;
        self.fd_local_subrs
            .get(fd)
            .map(|v| v.as_slice())
            .unwrap_or(&self.local_subrs)
    }

    /// Glyph id → CID, used when a CID font needs the reverse lookup.
    pub fn cid_for_gid(&self, gid: u16) -> Option<u16> {
        self.charset.get(usize::from(gid)).copied()
    }
}

// ---------------------------------------------------------------------------
// Container structures
// ---------------------------------------------------------------------------

fn is_sfnt(data: &[u8]) -> bool {
    matches!(
        data.first_chunk::<4>(),
        Some(b"OTTO") | Some(&[0x00, 0x01, 0x00, 0x00]) | Some(b"true") | Some(b"ttcf")
    )
}

/// Pull the `CFF ` table out of an OpenType wrapper.
fn extract_cff_table(data: &[u8]) -> Option<Vec<u8>> {
    let num_tables = u16::from_be_bytes(*data.get(4..6)?.first_chunk::<2>()?) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = data.get(rec..rec + 4)?;
        if tag == b"CFF " {
            let offset = u32::from_be_bytes(*data.get(rec + 8..rec + 12)?.first_chunk::<4>()?)
                as usize;
            let length = u32::from_be_bytes(*data.get(rec + 12..rec + 16)?.first_chunk::<4>()?)
                as usize;
            return data.get(offset..offset.checked_add(length)?).map(<[u8]>::to_vec);
        }
    }
    None
}

/// Read a CFF INDEX, returning each element's byte range and the offset just
/// past the structure.
fn read_index(data: &[u8], pos: usize) -> Option<(Vec<(usize, usize)>, usize)> {
    let count = u16::from_be_bytes(*data.get(pos..pos + 2)?.first_chunk::<2>()?) as usize;
    if count == 0 {
        return Some((Vec::new(), pos + 2));
    }
    let off_size = *data.get(pos + 2)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = pos + 3;
    let read_offset = |i: usize| -> Option<usize> {
        let at = offsets_start + i * off_size;
        let bytes = data.get(at..at + off_size)?;
        Some(bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize))
    };
    // Offsets are 1-based from the byte after the offset array.
    let data_start = offsets_start + (count + 1) * off_size - 1;
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        let start = data_start.checked_add(read_offset(i)?)?;
        let end = data_start.checked_add(read_offset(i + 1)?)?;
        if start > end || end > data.len() {
            return None;
        }
        items.push((start, end));
    }
    let end = data_start.checked_add(read_offset(count)?)?;
    Some((items, end))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Operator {
    Plain(u8),
    Escape(u8),
}

/// Parse a CFF DICT into operator → operand list.
fn parse_dict(data: &[u8]) -> HashMap<Operator, Vec<f64>> {
    let mut out = HashMap::new();
    let mut operands: Vec<f64> = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        match b0 {
            // Operators.
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    Operator::Escape(*data.get(i).unwrap_or(&0))
                } else {
                    Operator::Plain(b0)
                };
                out.insert(op, std::mem::take(&mut operands));
                i += 1;
            }
            28 => {
                let v = i16::from_be_bytes([
                    *data.get(i + 1).unwrap_or(&0),
                    *data.get(i + 2).unwrap_or(&0),
                ]);
                operands.push(v as f64);
                i += 3;
            }
            29 => {
                let v = i32::from_be_bytes([
                    *data.get(i + 1).unwrap_or(&0),
                    *data.get(i + 2).unwrap_or(&0),
                    *data.get(i + 3).unwrap_or(&0),
                    *data.get(i + 4).unwrap_or(&0),
                ]);
                operands.push(v as f64);
                i += 5;
            }
            30 => {
                // Real number: packed nibbles terminated by 0xf.
                let mut text = String::new();
                i += 1;
                'real: while i < data.len() {
                    let byte = data[i];
                    i += 1;
                    for nibble in [byte >> 4, byte & 0xf] {
                        match nibble {
                            0..=9 => text.push((b'0' + nibble) as char),
                            0xa => text.push('.'),
                            0xb => text.push('E'),
                            0xc => text.push_str("E-"),
                            0xe => text.push('-'),
                            0xf => break 'real,
                            _ => {}
                        }
                    }
                }
                operands.push(text.parse().unwrap_or(0.0));
            }
            32..=246 => {
                operands.push(b0 as f64 - 139.0);
                i += 1;
            }
            247..=250 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as f64;
                operands.push((b0 as f64 - 247.0) * 256.0 + b1 + 108.0);
                i += 2;
            }
            251..=254 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as f64;
                operands.push(-(b0 as f64 - 251.0) * 256.0 - b1 - 108.0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

fn read_private_subrs(data: &[u8], offset: usize, size: usize) -> Option<Vec<(usize, usize)>> {
    let end = offset.checked_add(size)?;
    let private = parse_dict(data.get(offset..end.min(data.len()))?);
    let subrs_offset = *private.get(&Operator::Plain(19))?.first()? as usize;
    let (subrs, _) = read_index(data, offset.checked_add(subrs_offset)?)?;
    Some(subrs)
}

fn read_fd_select(data: &[u8], offset: usize, num_glyphs: usize) -> Vec<u8> {
    let mut out = vec![0u8; num_glyphs];
    match data.get(offset) {
        Some(0) => {
            for (gid, slot) in out.iter_mut().enumerate() {
                *slot = data.get(offset + 1 + gid).copied().unwrap_or(0);
            }
        }
        Some(3) => {
            let Some(chunk) = data.get(offset + 1..offset + 3) else {
                return out;
            };
            let n_ranges = u16::from_be_bytes([chunk[0], chunk[1]]) as usize;
            let mut pos = offset + 3;
            let mut ranges = Vec::with_capacity(n_ranges);
            for _ in 0..n_ranges {
                let Some(first) = data.get(pos..pos + 2) else { break };
                let first = u16::from_be_bytes([first[0], first[1]]) as usize;
                let fd = data.get(pos + 2).copied().unwrap_or(0);
                ranges.push((first, fd));
                pos += 3;
            }
            let sentinel = data
                .get(pos..pos + 2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
                .unwrap_or(num_glyphs);
            for i in 0..ranges.len() {
                let start = ranges[i].0;
                let end = ranges.get(i + 1).map(|r| r.0).unwrap_or(sentinel);
                for slot in out.iter_mut().take(end.min(num_glyphs)).skip(start) {
                    *slot = ranges[i].1;
                }
            }
        }
        _ => {}
    }
    out
}

fn read_charset(data: &[u8], offset: usize, num_glyphs: usize) -> Vec<u16> {
    // Predefined charsets (0 = ISOAdobe) map gid → SID identically for the
    // standard ordering, which is the right answer for 0 and a harmless
    // approximation for the two expert sets.
    if offset <= 2 {
        return (0..num_glyphs as u16).collect();
    }
    let mut out = Vec::with_capacity(num_glyphs);
    out.push(0); // .notdef
    match data.get(offset) {
        Some(0) => {
            let mut pos = offset + 1;
            while out.len() < num_glyphs {
                let Some(b) = data.get(pos..pos + 2) else { break };
                out.push(u16::from_be_bytes([b[0], b[1]]));
                pos += 2;
            }
        }
        Some(format @ (1 | 2)) => {
            let wide = *format == 2;
            let mut pos = offset + 1;
            while out.len() < num_glyphs {
                let Some(first) = data.get(pos..pos + 2) else { break };
                let first = u16::from_be_bytes([first[0], first[1]]);
                let n_left = if wide {
                    let Some(b) = data.get(pos + 2..pos + 4) else { break };
                    pos += 4;
                    u16::from_be_bytes([b[0], b[1]]) as usize
                } else {
                    let Some(b) = data.get(pos + 2) else { break };
                    pos += 3;
                    *b as usize
                };
                for i in 0..=n_left {
                    if out.len() >= num_glyphs {
                        break;
                    }
                    out.push(first.saturating_add(i as u16));
                }
            }
        }
        _ => {}
    }
    out.resize(num_glyphs, 0);
    out
}

/// Resolve a SID to its glyph name: standard strings first, then the font's
/// own string INDEX.
fn sid_name(sid: u16, strings: &[(usize, usize)], data: &[u8]) -> Option<String> {
    let sid = sid as usize;
    if sid < STANDARD_STRINGS.len() {
        return Some(STANDARD_STRINGS[sid].to_string());
    }
    let (start, end) = *strings.get(sid - STANDARD_STRINGS.len())?;
    std::str::from_utf8(data.get(start..end)?).ok().map(str::to_string)
}

// ---------------------------------------------------------------------------
// Type 2 charstring interpreter
// ---------------------------------------------------------------------------

fn bias(count: usize) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

struct CharStringCtx<'a> {
    font: &'a CffFont,
    local: &'a [(usize, usize)],
    path: Path,
    stack: Vec<f64>,
    x: f64,
    y: f64,
    stems: usize,
    width_parsed: bool,
    open: bool,
    depth: u8,
    /// Transient array for `put`/`get`; rarely used but cheap to support.
    trans: Vec<f64>,
}

impl CharStringCtx<'_> {
    fn finish(mut self) -> Option<Path> {
        if self.open {
            self.path.close();
        }
        if self.path.subpaths.is_empty() {
            None
        } else {
            Some(self.path)
        }
    }

    fn move_to(&mut self, dx: f64, dy: f64) {
        if self.open {
            self.path.close();
        }
        self.x += dx;
        self.y += dy;
        self.path.move_to(self.x, self.y);
        self.open = true;
    }

    fn line_to(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.path.line_to(self.x, self.y);
    }

    fn curve_to(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        let x1 = self.x + dx1;
        let y1 = self.y + dy1;
        let x2 = x1 + dx2;
        let y2 = y1 + dy2;
        self.x = x2 + dx3;
        self.y = y2 + dy3;
        self.path.curve_to(x1, y1, x2, y2, self.x, self.y);
    }

    /// A leading odd argument on the first stack-clearing operator is the
    /// glyph width, not a coordinate. Dropping it is what keeps the first
    /// contour from starting in the wrong place.
    fn take_width(&mut self, even: bool) {
        if self.width_parsed {
            return;
        }
        self.width_parsed = true;
        let odd = self.stack.len() % 2 == 1;
        if (even && odd) || (!even && !odd && !self.stack.is_empty()) {
            self.stack.remove(0);
        }
    }

    fn count_stems(&mut self) {
        if !self.width_parsed && self.stack.len() % 2 == 1 {
            self.stack.remove(0);
        }
        self.width_parsed = true;
        self.stems += self.stack.len() / 2;
        self.stack.clear();
    }

    fn run(&mut self, range: (usize, usize)) -> bool {
        if self.depth > 10 {
            return true;
        }
        let data = &self.font.data;
        let mut i = range.0;
        while i < range.1 && i < data.len() {
            let b0 = data[i];
            i += 1;
            match b0 {
                // ---- operands ------------------------------------------
                32..=246 => self.stack.push(b0 as f64 - 139.0),
                247..=250 => {
                    let b1 = *data.get(i).unwrap_or(&0) as f64;
                    i += 1;
                    self.stack.push((b0 as f64 - 247.0) * 256.0 + b1 + 108.0);
                }
                251..=254 => {
                    let b1 = *data.get(i).unwrap_or(&0) as f64;
                    i += 1;
                    self.stack.push(-(b0 as f64 - 251.0) * 256.0 - b1 - 108.0);
                }
                28 => {
                    let v = i16::from_be_bytes([
                        *data.get(i).unwrap_or(&0),
                        *data.get(i + 1).unwrap_or(&0),
                    ]);
                    i += 2;
                    self.stack.push(v as f64);
                }
                255 => {
                    // 16.16 fixed point.
                    let v = i32::from_be_bytes([
                        *data.get(i).unwrap_or(&0),
                        *data.get(i + 1).unwrap_or(&0),
                        *data.get(i + 2).unwrap_or(&0),
                        *data.get(i + 3).unwrap_or(&0),
                    ]);
                    i += 4;
                    self.stack.push(v as f64 / 65536.0);
                }

                // ---- hints ----------------------------------------------
                1 | 3 | 18 | 23 => self.count_stems(),
                19 | 20 => {
                    self.count_stems();
                    i += self.stems.div_ceil(8);
                }

                // ---- path construction ----------------------------------
                21 => {
                    self.take_width(true);
                    let dy = self.stack.pop().unwrap_or(0.0);
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.move_to(dx, dy);
                    self.stack.clear();
                }
                22 => {
                    self.take_width(false);
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.move_to(dx, 0.0);
                    self.stack.clear();
                }
                4 => {
                    self.take_width(false);
                    let dy = self.stack.pop().unwrap_or(0.0);
                    self.move_to(0.0, dy);
                    self.stack.clear();
                }
                5 => {
                    let args = std::mem::take(&mut self.stack);
                    for pair in args.chunks_exact(2) {
                        self.line_to(pair[0], pair[1]);
                    }
                }
                6 | 7 => {
                    // Alternating horizontal/vertical lines.
                    let args = std::mem::take(&mut self.stack);
                    let mut horizontal = b0 == 6;
                    for &d in &args {
                        if horizontal {
                            self.line_to(d, 0.0);
                        } else {
                            self.line_to(0.0, d);
                        }
                        horizontal = !horizontal;
                    }
                }
                8 => {
                    let args = std::mem::take(&mut self.stack);
                    for c in args.chunks_exact(6) {
                        self.curve_to(c[0], c[1], c[2], c[3], c[4], c[5]);
                    }
                }
                24 => {
                    // rcurveline: curves then one line.
                    let args = std::mem::take(&mut self.stack);
                    let curves = (args.len().saturating_sub(2)) / 6 * 6;
                    for c in args[..curves].chunks_exact(6) {
                        self.curve_to(c[0], c[1], c[2], c[3], c[4], c[5]);
                    }
                    if let Some(rest) = args.get(curves..curves + 2) {
                        self.line_to(rest[0], rest[1]);
                    }
                }
                25 => {
                    // rlinecurve: lines then one curve.
                    let args = std::mem::take(&mut self.stack);
                    let lines = args.len().saturating_sub(6);
                    let lines = lines - lines % 2;
                    for pair in args[..lines].chunks_exact(2) {
                        self.line_to(pair[0], pair[1]);
                    }
                    if let Some(c) = args.get(lines..lines + 6) {
                        self.curve_to(c[0], c[1], c[2], c[3], c[4], c[5]);
                    }
                }
                26 | 27 => {
                    // vvcurveto / hhcurveto, with an optional leading cross-
                    // axis delta applied to the first curve only.
                    let mut args = std::mem::take(&mut self.stack);
                    let mut d1 = 0.0;
                    if args.len() % 4 == 1 {
                        d1 = args.remove(0);
                    }
                    for c in args.chunks_exact(4) {
                        if b0 == 26 {
                            self.curve_to(d1, c[0], c[1], c[2], 0.0, c[3]);
                        } else {
                            self.curve_to(c[0], d1, c[1], c[2], c[3], 0.0);
                        }
                        d1 = 0.0;
                    }
                }
                30 | 31 => {
                    // vhcurveto / hvcurveto: alternating start tangents, with
                    // an optional trailing delta on the final curve.
                    let args = std::mem::take(&mut self.stack);
                    let mut horizontal = b0 == 31;
                    let mut k = 0;
                    while k + 4 <= args.len() {
                        let last = k + 8 > args.len();
                        let extra = if last && args.len() - k == 5 {
                            args[k + 4]
                        } else {
                            0.0
                        };
                        if horizontal {
                            self.curve_to(args[k], 0.0, args[k + 1], args[k + 2], extra, args[k + 3]);
                        } else {
                            self.curve_to(0.0, args[k], args[k + 1], args[k + 2], args[k + 3], extra);
                        }
                        horizontal = !horizontal;
                        k += 4;
                    }
                }

                // ---- subroutines ----------------------------------------
                10 | 29 => {
                    let subrs = if b0 == 10 {
                        self.local
                    } else {
                        &self.font.global_subrs
                    };
                    let Some(index) = self.stack.pop() else { continue };
                    let index = index as i32 + bias(subrs.len());
                    if index >= 0 {
                        if let Some(&sub) = subrs.get(index as usize) {
                            self.depth += 1;
                            let stop = self.run(sub);
                            self.depth -= 1;
                            if stop {
                                return true;
                            }
                        }
                    }
                }
                11 => return false,
                14 => {
                    // endchar. A 4-argument form is `seac`-style accent
                    // composition, which is rare enough to skip rather than
                    // mis-draw.
                    self.take_width(true);
                    if self.open {
                        self.path.close();
                        self.open = false;
                    }
                    return true;
                }

                // ---- escaped operators ----------------------------------
                12 => {
                    let b1 = *data.get(i).unwrap_or(&0);
                    i += 1;
                    match b1 {
                        35 => {
                            // flex: two curves, fd argument ignored.
                            let a = std::mem::take(&mut self.stack);
                            if a.len() >= 13 {
                                self.curve_to(a[0], a[1], a[2], a[3], a[4], a[5]);
                                self.curve_to(a[6], a[7], a[8], a[9], a[10], a[11]);
                            }
                        }
                        34 => {
                            // hflex: both curves horizontal, y returns home.
                            let a = std::mem::take(&mut self.stack);
                            if a.len() >= 7 {
                                self.curve_to(a[0], 0.0, a[1], a[2], a[3], 0.0);
                                self.curve_to(a[4], 0.0, a[5], -a[2], a[6], 0.0);
                            }
                        }
                        36 => {
                            // hflex1
                            let a = std::mem::take(&mut self.stack);
                            if a.len() >= 9 {
                                let start_y = self.y;
                                self.curve_to(a[0], a[1], a[2], a[3], a[4], 0.0);
                                let dy_last = start_y - (self.y + a[6] + a[8].mul_add(0.0, 0.0));
                                self.curve_to(a[5], 0.0, a[6], a[7], a[8], dy_last - a[7]);
                            }
                        }
                        37 => {
                            // flex1: the final point returns to the start in
                            // whichever axis moved least.
                            let a = std::mem::take(&mut self.stack);
                            if a.len() >= 11 {
                                let start_x = self.x;
                                let start_y = self.y;
                                let dx = a[0] + a[2] + a[4] + a[6] + a[8];
                                let dy = a[1] + a[3] + a[5] + a[7] + a[9];
                                self.curve_to(a[0], a[1], a[2], a[3], a[4], a[5]);
                                if dx.abs() > dy.abs() {
                                    let last_y = start_y - (self.y + a[7]);
                                    self.curve_to(a[6], a[7], a[8], a[9], a[10], last_y - a[9]);
                                } else {
                                    let last_x = start_x - (self.x + a[6] + a[8]);
                                    self.curve_to(a[6], a[7], a[8], a[9], last_x, a[10]);
                                }
                            }
                        }
                        // Arithmetic and storage operators: supported enough
                        // that a font using them does not derail the parse.
                        20 => {
                            let j = self.stack.pop().unwrap_or(0.0) as usize;
                            let v = self.stack.pop().unwrap_or(0.0);
                            if self.trans.len() <= j && j < 32 {
                                self.trans.resize(j + 1, 0.0);
                            }
                            if let Some(slot) = self.trans.get_mut(j) {
                                *slot = v;
                            }
                        }
                        21 => {
                            let j = self.stack.pop().unwrap_or(0.0) as usize;
                            self.stack.push(self.trans.get(j).copied().unwrap_or(0.0));
                        }
                        _ => self.stack.clear(),
                    }
                }
                _ => self.stack.clear(),
            }
        }
        false
    }
}

/// Standard-encoding glyph name for a character.
///
/// CFF addresses glyphs by name, so a simple (non-CID) font needs a name to
/// look up. For codes 32..126 the standard strings are laid out in exactly
/// StandardEncoding order starting at SID 1, so the mapping is arithmetic
/// rather than another table.
pub fn standard_name_for_char(ch: char) -> Option<&'static str> {
    let code = ch as u32;
    if !(32..=126).contains(&code) {
        return None;
    }
    STANDARD_STRINGS.get((code - 31) as usize).copied()
}

/// The 391 predefined CFF strings. Only the name-addressable range matters
/// here — glyph names for simple fonts.
const STANDARD_STRINGS: [&str; 391] = [
    ".notdef", "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand",
    "quoteright", "parenleft", "parenright", "asterisk", "plus", "comma", "hyphen", "period",
    "slash", "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    "colon", "semicolon", "less", "equal", "greater", "question", "at", "A", "B", "C", "D", "E",
    "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X",
    "Y", "Z", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore",
    "quoteleft", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
    "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "braceleft", "bar", "braceright",
    "asciitilde", "exclamdown", "cent", "sterling", "fraction", "yen", "florin", "section",
    "currency", "quotesingle", "quotedblleft", "guillemotleft", "guilsinglleft", "guilsinglright",
    "fi", "fl", "endash", "dagger", "daggerdbl", "periodcentered", "paragraph", "bullet",
    "quotesinglbase", "quotedblbase", "quotedblright", "guillemotright", "ellipsis", "perthousand",
    "questiondown", "grave", "acute", "circumflex", "tilde", "macron", "breve", "dotaccent",
    "dieresis", "ring", "cedilla", "hungarumlaut", "ogonek", "caron", "emdash", "AE",
    "ordfeminine", "Lslash", "Oslash", "OE", "ordmasculine", "ae", "dotlessi", "lslash", "oslash",
    "oe", "germandbls", "onesuperior", "logicalnot", "mu", "trademark", "Eth", "onehalf",
    "plusminus", "Thorn", "onequarter", "divide", "brokenbar", "degree", "thorn",
    "threequarters", "twosuperior", "registered", "minus", "eth", "multiply", "threesuperior",
    "copyright", "Aacute", "Acircumflex", "Adieresis", "Agrave", "Aring", "Atilde", "Ccedilla",
    "Eacute", "Ecircumflex", "Edieresis", "Egrave", "Iacute", "Icircumflex", "Idieresis",
    "Igrave", "Ntilde", "Oacute", "Ocircumflex", "Odieresis", "Ograve", "Otilde", "Scaron",
    "Uacute", "Ucircumflex", "Udieresis", "Ugrave", "Yacute", "Ydieresis", "Zcaron", "aacute",
    "acircumflex", "adieresis", "agrave", "aring", "atilde", "ccedilla", "eacute", "ecircumflex",
    "edieresis", "egrave", "iacute", "icircumflex", "idieresis", "igrave", "ntilde", "oacute",
    "ocircumflex", "odieresis", "ograve", "otilde", "scaron", "uacute", "ucircumflex",
    "udieresis", "ugrave", "yacute", "ydieresis", "zcaron", "exclamsmall", "Hungarumlautsmall",
    "dollaroldstyle", "dollarsuperior", "ampersandsmall", "Acutesmall", "parenleftsuperior",
    "parenrightsuperior", "twodotenleader", "onedotenleader", "zerooldstyle", "oneoldstyle",
    "twooldstyle", "threeoldstyle", "fouroldstyle", "fiveoldstyle", "sixoldstyle",
    "sevenoldstyle", "eightoldstyle", "nineoldstyle", "commasuperior",
    "threequartersemdash", "periodsuperior", "questionsmall", "asuperior", "bsuperior",
    "centsuperior", "dsuperior", "esuperior", "isuperior", "lsuperior", "msuperior",
    "nsuperior", "osuperior", "rsuperior", "ssuperior", "tsuperior", "ff", "ffi", "ffl",
    "parenleftinferior", "parenrightinferior", "Circumflexsmall", "hyphensuperior",
    "Gravesmall", "Asmall", "Bsmall", "Csmall", "Dsmall", "Esmall", "Fsmall", "Gsmall", "Hsmall",
    "Ismall", "Jsmall", "Ksmall", "Lsmall", "Msmall", "Nsmall", "Osmall", "Psmall", "Qsmall",
    "Rsmall", "Ssmall", "Tsmall", "Usmall", "Vsmall", "Wsmall", "Xsmall", "Ysmall", "Zsmall",
    "colonmonetary", "onefitted", "rupiah", "Tildesmall", "exclamdownsmall", "centoldstyle",
    "Lslashsmall", "Scaronsmall", "Zcaronsmall", "Dieresissmall", "Brevesmall", "Caronsmall",
    "Dotaccentsmall", "Macronsmall", "figuredash", "hypheninferior", "Ogoneksmall",
    "Ringsmall", "Cedillasmall", "questiondownsmall", "oneeighth", "threeeighths",
    "fiveeighths", "seveneighths", "onethird", "twothirds", "zerosuperior", "foursuperior",
    "fivesuperior", "sixsuperior", "sevensuperior", "eightsuperior", "ninesuperior",
    "zeroinferior", "oneinferior", "twoinferior", "threeinferior", "fourinferior",
    "fiveinferior", "sixinferior", "seveninferior", "eightinferior", "nineinferior",
    "centinferior", "dollarinferior", "periodinferior", "commainferior", "Agravesmall",
    "Aacutesmall", "Acircumflexsmall", "Atildesmall", "Adieresissmall", "Aringsmall",
    "AEsmall", "Ccedillasmall", "Egravesmall", "Eacutesmall", "Ecircumflexsmall",
    "Edieresissmall", "Igravesmall", "Iacutesmall", "Icircumflexsmall", "Idieresissmall",
    "Ethsmall", "Ntildesmall", "Ogravesmall", "Oacutesmall", "Ocircumflexsmall",
    "Otildesmall", "Odieresissmall", "OEsmall", "Oslashsmall", "Ugravesmall", "Uacutesmall",
    "Ucircumflexsmall", "Udieresissmall", "Yacutesmall", "Thornsmall", "Ydieresissmall",
    "001.000", "001.001", "001.002", "001.003", "Black", "Bold", "Book", "Light", "Medium",
    "Regular", "Roman", "Semibold",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subr_bias_follows_the_spec() {
        assert_eq!(bias(0), 107);
        assert_eq!(bias(1239), 107);
        assert_eq!(bias(1240), 1131);
        assert_eq!(bias(33899), 1131);
        assert_eq!(bias(33900), 32768);
    }

    #[test]
    fn parses_dict_integer_encodings() {
        // 139 -> 0, then operator 17 (CharStrings).
        let dict = parse_dict(&[139, 17]);
        assert_eq!(dict.get(&Operator::Plain(17)), Some(&vec![0.0]));

        // 28 introduces a 16-bit signed integer.
        let dict = parse_dict(&[28, 0x01, 0x00, 15]);
        assert_eq!(dict.get(&Operator::Plain(15)), Some(&vec![256.0]));

        // 247 x: (247-247)*256 + x + 108
        let dict = parse_dict(&[247, 10, 15]);
        assert_eq!(dict.get(&Operator::Plain(15)), Some(&vec![118.0]));

        // 251 x: -(251-251)*256 - x - 108
        let dict = parse_dict(&[251, 10, 15]);
        assert_eq!(dict.get(&Operator::Plain(15)), Some(&vec![-118.0]));
    }

    #[test]
    fn parses_dict_real_numbers() {
        // 30 introduces nibble-packed reals: -0.5 terminated by 0xf.
        let dict = parse_dict(&[30, 0xe0, 0xa5, 0xff, 15]);
        let value = dict.get(&Operator::Plain(15)).unwrap()[0];
        assert!((value + 0.5).abs() < 1e-9, "got {value}");
    }

    #[test]
    fn escaped_operators_are_distinct_from_plain_ones() {
        let dict = parse_dict(&[139, 12, 30]);
        assert!(dict.contains_key(&Operator::Escape(30)));
        assert!(!dict.contains_key(&Operator::Plain(30)));
    }

    #[test]
    fn empty_index_reports_its_end() {
        let (items, end) = read_index(&[0, 0], 0).unwrap();
        assert!(items.is_empty());
        assert_eq!(end, 2);
    }

    #[test]
    fn reads_a_two_element_index() {
        // count=2, offSize=1, offsets 1,3,5 then "ab" "cd".
        let data = [0, 2, 1, 1, 3, 5, b'a', b'b', b'c', b'd'];
        let (items, end) = read_index(&data, 0).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(&data[items[0].0..items[0].1], b"ab");
        assert_eq!(&data[items[1].0..items[1].1], b"cd");
        assert_eq!(end, data.len());
    }

    #[test]
    fn rejects_an_index_that_runs_past_the_buffer() {
        // Offsets claim more data than exists.
        let data = [0, 1, 1, 1, 200];
        assert!(read_index(&data, 0).is_none());
    }

    #[test]
    fn charset_format_zero_maps_gids_to_sids() {
        // Offsets 0-2 are reserved for the predefined charsets, so a real
        // table always sits past them: pad to 3, format 0, then the SIDs for
        // gids 1 and 2.
        let data = [0xff, 0xff, 0xff, 0, 0, 5, 0, 9];
        let charset = read_charset(&data, 3, 3);
        assert_eq!(charset, vec![0, 5, 9]);
    }

    #[test]
    fn charset_format_one_expands_ranges() {
        // format 1: first SID 10, nLeft 2 -> 10, 11, 12 for gids 1..3.
        let data = [0xff, 0xff, 0xff, 1, 0, 10, 2];
        let charset = read_charset(&data, 3, 4);
        assert_eq!(charset, vec![0, 10, 11, 12]);
    }

    #[test]
    fn predefined_charset_is_the_identity() {
        assert_eq!(read_charset(&[], 0, 4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn fd_select_format_three_covers_every_glyph() {
        // format 3, 2 ranges: gid 0.. -> fd 0, gid 2.. -> fd 1, sentinel 4.
        let data = [3, 0, 2, 0, 0, 0, 0, 2, 1, 0, 4];
        assert_eq!(read_fd_select(&data, 0, 4), vec![0, 0, 1, 1]);
    }

    #[test]
    fn predefined_charset_offsets_are_not_read_as_data() {
        // 0, 1 and 2 name the ISOAdobe/Expert charsets rather than pointing
        // at a table; reading them as offsets would decode random bytes.
        for offset in 0..=2 {
            assert_eq!(read_charset(&[9, 9, 9, 9], offset, 3), vec![0, 1, 2]);
        }
    }

    #[test]
    fn standard_strings_cover_the_predefined_sids() {
        assert_eq!(STANDARD_STRINGS.len(), 391);
        assert_eq!(STANDARD_STRINGS[0], ".notdef");
        assert_eq!(STANDARD_STRINGS[1], "space");
        assert_eq!(sid_name(34, &[], &[]).as_deref(), Some("A"));
    }

    #[test]
    fn sid_beyond_the_standard_set_reads_the_string_index() {
        let data = b"Custom".to_vec();
        let strings = vec![(0usize, 6usize)];
        assert_eq!(
            sid_name(391, &strings, &data).as_deref(),
            Some("Custom")
        );
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(CffFont::parse(vec![]).is_none());
        assert!(CffFont::parse(vec![1, 0, 4, 1]).is_none());
        assert!(CffFont::parse(vec![0xff; 64]).is_none());
    }
}
