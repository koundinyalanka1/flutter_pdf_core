//! Minimal TrueType/OpenType parser: enough to turn a glyph id into an
//! outline, and a character code into a glyph id.
//!
//! Covers `head`, `maxp`, `loca`, `glyf` (simple *and* composite glyphs),
//! `cmap` formats 0, 4, 6 and 12, `hmtx`, `post` and `name` — what PDF font
//! subsets embedded as `/FontFile2` use, and what a system or bundled
//! fallback face needs. Any face of a collection (`.ttc`) can be opened, and
//! an OpenType face with CFF outlines (`OTTO`) draws through the CFF
//! interpreter, as CJK system fonts require.

use std::collections::HashMap;

use super::cff::CffFont;
use crate::geom::Path;

#[derive(Debug)]
pub struct TrueTypeFont {
    data: Vec<u8>,
    tables: HashMap<[u8; 4], (usize, usize)>,
    pub units_per_em: f64,
    num_glyphs: u16,
    /// Byte offsets into `glyf`, `num_glyphs + 1` entries.
    loca: Vec<u32>,
    /// Unicode (or symbol) code point -> glyph id.
    cmap: HashMap<u32, u16>,
    /// True when the selected cmap subtable is a (3,0) symbol mapping, whose
    /// codes live in the 0xF000 private-use block.
    symbolic_cmap: bool,
    /// Glyph name → glyph id from the `post` table, built on first use. Large
    /// CJK faces carry tens of thousands of names nobody asks for.
    post_names: std::sync::OnceLock<HashMap<String, u16>>,
    /// CFF outlines, for an `OTTO` face that has no `glyf`.
    cff: Option<Box<CffFont>>,
}

impl std::fmt::Debug for CffFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CffFont")
            .field("glyphs", &self.num_glyphs())
            .finish()
    }
}

/// How many faces a font file holds: the count of a collection, else one.
pub fn face_count(data: &[u8]) -> usize {
    if data.get(0..4) == Some(b"ttcf") {
        read_u32(data, 8).map(|n| n as usize).unwrap_or(0).min(256)
    } else {
        usize::from(data.len() >= 12)
    }
}

/// Table tag → (offset, length) in the font data.
type Tables = HashMap<[u8; 4], (usize, usize)>;

/// The sfnt version tag and table directory of face `index`.
fn table_directory(data: &[u8], index: usize) -> Option<(u32, Tables)> {
    if data.len() < 12 {
        return None;
    }
    // In a collection the header lists one offset per face; offsets in each
    // face's directory are relative to the start of the whole file.
    let base = if &data[0..4] == b"ttcf" {
        if index >= face_count(data) {
            return None;
        }
        read_u32(data, 12 + index * 4)? as usize
    } else if index == 0 {
        0
    } else {
        return None;
    };
    let tag = read_u32(data, base)?;
    // 0x00010000 / 'true' carry TrueType outlines, 'OTTO' carries CFF.
    if tag != 0x0001_0000 && tag != 0x7472_7565 && tag != 0x4F54_544F {
        return None;
    }
    let table_count = read_u16(data, base + 4)? as usize;
    let mut tables = HashMap::with_capacity(table_count);
    for i in 0..table_count {
        let record = base + 12 + i * 16;
        if record + 16 > data.len() {
            break;
        }
        let mut name = [0u8; 4];
        name.copy_from_slice(&data[record..record + 4]);
        let offset = read_u32(data, record + 8)? as usize;
        let length = read_u32(data, record + 12)? as usize;
        if offset <= data.len() {
            tables.insert(name, (offset, length.min(data.len() - offset)));
        }
    }
    Some((tag, tables))
}

impl TrueTypeFont {
    /// Parse a font as embedded in a PDF: the first face of whatever it is.
    pub fn parse(data: Vec<u8>) -> Option<TrueTypeFont> {
        Self::parse_face(data, 0)
    }

    /// Parse face `index` of a font file or collection.
    pub fn parse_face(data: Vec<u8>, index: usize) -> Option<TrueTypeFont> {
        let (tag, mut tables) = table_directory(&data, index)?;

        let (head_offset, _) = *tables.get(b"head")?;
        let units_per_em = read_u16(&data, head_offset + 18)? as f64;
        let index_to_loc_format = read_i16(&data, head_offset + 50)?;

        let (maxp_offset, _) = *tables.get(b"maxp")?;
        let num_glyphs = read_u16(&data, maxp_offset + 4)?;

        let loca = tables
            .get(b"loca")
            .and_then(|&(offset, length)| {
                read_loca(&data, offset, length, num_glyphs, index_to_loc_format)
            })
            .unwrap_or_default();

        // An OTTO face draws through its CFF table. Copy that out, then keep
        // only the small tables this struct still reads, so a 20 MB CJK face
        // is not held twice.
        let mut data = data;
        let mut cff = None;
        if tag == 0x4F54_544F && !tables.contains_key(b"glyf") {
            let &(offset, length) = tables.get(b"CFF ")?;
            cff = Some(Box::new(CffFont::parse(
                data.get(offset..offset + length)?.to_vec(),
            )?));
            let mut compact = Vec::new();
            let mut kept = HashMap::new();
            for tag in [
                b"head", b"maxp", b"hhea", b"hmtx", b"cmap", b"post", b"name", b"OS/2",
            ] {
                if let Some(&(offset, length)) = tables.get(tag) {
                    if let Some(bytes) = data.get(offset..offset + length) {
                        kept.insert(*tag, (compact.len(), length));
                        compact.extend_from_slice(bytes);
                        // Keep tables 4-aligned like the format expects.
                        while compact.len() % 4 != 0 {
                            compact.push(0);
                        }
                    }
                }
            }
            data = compact;
            tables = kept;
        }

        let mut font = TrueTypeFont {
            units_per_em: if units_per_em > 0.0 {
                units_per_em
            } else {
                1000.0
            },
            num_glyphs,
            loca,
            cmap: HashMap::new(),
            symbolic_cmap: false,
            post_names: std::sync::OnceLock::new(),
            cff,
            tables,
            data,
        };
        font.load_cmap();
        Some(font)
    }

    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// The face's family name from its `name` table (Unicode or Mac Roman
    /// records), for matching a substitute's style to the original's.
    pub fn family_name(&self) -> Option<String> {
        // Typographic family first: "Noto Sans CJK JP" rather than the
        // style-linked "Noto Sans CJK JP Regular".
        self.name_record(16).or_else(|| self.name_record(1))
    }

    /// The PostScript name (name ID 6), e.g. `HiraginoSans-W3`.
    pub fn postscript_name(&self) -> Option<String> {
        self.name_record(6)
    }

    fn name_record(&self, wanted: u16) -> Option<String> {
        let &(table, length) = self.tables.get(b"name")?;
        let count = usize::from(read_u16(&self.data, table + 2)?);
        let strings = table + usize::from(read_u16(&self.data, table + 4)?);
        let mut fallback = None;
        for i in 0..count {
            let record = table + 6 + i * 12;
            if record + 12 > table + length {
                break;
            }
            let platform = read_u16(&self.data, record)?;
            let id = read_u16(&self.data, record + 6)?;
            if id != wanted {
                continue;
            }
            let len = usize::from(read_u16(&self.data, record + 8)?);
            let offset = usize::from(read_u16(&self.data, record + 10)?);
            let bytes = self.data.get(strings + offset..strings + offset + len)?;
            match platform {
                // Windows and Unicode platforms: UTF-16BE.
                0 | 3 => {
                    let units: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    return Some(String::from_utf16_lossy(&units));
                }
                // Macintosh: Roman, close enough to ASCII for family names.
                1 if fallback.is_none() => {
                    fallback = Some(bytes.iter().map(|&b| b as char).collect());
                }
                _ => {}
            }
        }
        fallback
    }

    /// `OS/2` weight class (400 regular, 700 bold) and italic flag.
    pub fn weight_and_italic(&self) -> (u16, bool) {
        let Some(&(os2, _)) = self.tables.get(b"OS/2") else {
            return (400, false);
        };
        let weight = read_u16(&self.data, os2 + 4).unwrap_or(400);
        let selection = read_u16(&self.data, os2 + 62).unwrap_or(0);
        (weight, selection & 1 != 0)
    }

    /// Whether the face has serifs, from `OS/2` sFamilyClass (classes 1–7
    /// are serif designs, 8 is sans) — `None` when the font does not say.
    pub fn is_serif(&self) -> Option<bool> {
        let &(os2, _) = self.tables.get(b"OS/2")?;
        let class = read_i16(&self.data, os2 + 30)? >> 8;
        match class {
            1..=7 => Some(true),
            8 => Some(false),
            _ => None,
        }
    }

    /// Every code point the face maps to a glyph, as sorted ranges.
    pub fn coverage(&self) -> Vec<(u32, u32)> {
        let mut points: Vec<u32> = self.cmap.keys().copied().collect();
        points.sort_unstable();
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for point in points {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == point => *end = point,
                _ => ranges.push((point, point)),
            }
        }
        ranges
    }

    /// Advance width for a glyph, in font units.
    ///
    /// `hmtx` stores one record per glyph only up to `numberOfHMetrics`;
    /// every glyph beyond that reuses the last record's advance, which is how
    /// monospaced tails are compressed.
    pub fn advance(&self, glyph_id: u16) -> Option<f64> {
        let (hhea, _) = *self.tables.get(b"hhea")?;
        let metric_count = read_u16(&self.data, hhea + 34)?;
        if metric_count == 0 {
            return None;
        }
        let (hmtx, length) = *self.tables.get(b"hmtx")?;
        let index = glyph_id.min(metric_count - 1) as usize;
        let at = hmtx + index * 4;
        if at + 2 > hmtx + length {
            return None;
        }
        read_u16(&self.data, at).map(f64::from)
    }

    /// Whether the face can draw anything — TrueType or CFF outlines.
    pub fn has_outlines(&self) -> bool {
        self.has_glyf() || self.cff.is_some()
    }

    /// Whether the face carries TrueType (`glyf`) outlines specifically.
    pub fn has_glyf(&self) -> bool {
        !self.loca.is_empty() && self.tables.contains_key(b"glyf")
    }

    /// Glyph id for a Unicode code point, if the font's cmap has one.
    pub fn glyph_for_char(&self, ch: u32) -> Option<u16> {
        if let Some(&gid) = self.cmap.get(&ch) {
            return Some(gid);
        }
        if self.symbolic_cmap {
            // Symbol fonts map 0x20..0xFF into 0xF020..0xF0FF.
            if let Some(&gid) = self.cmap.get(&(0xF000 + (ch & 0xFF))) {
                return Some(gid);
            }
        }
        None
    }

    /// Glyph id for a PostScript glyph name, from the `post` table.
    ///
    /// This is how a simple font's `/Differences` names reach glyphs the cmap
    /// does not expose — legacy Indic and symbol TrueType fonts hang their
    /// glyphs on borrowed Latin names, or on names with no Unicode at all.
    pub fn gid_for_name(&self, name: &str) -> Option<u16> {
        self.post_names
            .get_or_init(|| self.read_post_names())
            .get(name)
            .copied()
    }

    /// `post` formats 1.0 (the 258 standard Macintosh names, in order) and
    /// 2.0 (an index per glyph into those names or the table's own Pascal
    /// strings). Format 3.0 carries no names, so it yields an empty map.
    fn read_post_names(&self) -> HashMap<String, u16> {
        let mut names = HashMap::new();
        let Some(&(post, length)) = self.tables.get(b"post") else {
            return names;
        };
        let end = post + length;
        match read_u32(&self.data, post) {
            Some(0x0001_0000) => {
                let count = usize::from(self.num_glyphs).min(MAC_GLYPH_NAMES.len());
                for (gid, name) in MAC_GLYPH_NAMES.iter().enumerate().take(count) {
                    names.entry((*name).to_owned()).or_insert(gid as u16);
                }
            }
            Some(0x0002_0000) => {
                let Some(count) = read_u16(&self.data, post + 32) else {
                    return names;
                };
                let indices = post + 34;
                // Custom names follow the index array, in index order.
                let mut custom = Vec::new();
                let mut at = indices + usize::from(count) * 2;
                while at < end {
                    let Some(&len) = self.data.get(at) else { break };
                    let Some(bytes) = self.data.get(at + 1..at + 1 + usize::from(len)) else {
                        break;
                    };
                    custom.push(String::from_utf8_lossy(bytes).into_owned());
                    at += 1 + usize::from(len);
                }
                for gid in 0..count {
                    let Some(index) = read_u16(&self.data, indices + usize::from(gid) * 2) else {
                        break;
                    };
                    let name = match usize::from(index) {
                        i if i < MAC_GLYPH_NAMES.len() => MAC_GLYPH_NAMES[i].to_owned(),
                        i => match custom.get(i - MAC_GLYPH_NAMES.len()) {
                            Some(name) => name.clone(),
                            None => continue,
                        },
                    };
                    // Several glyphs may share a name; the first one wins,
                    // matching what other readers resolve.
                    names.entry(name).or_insert(gid);
                }
            }
            _ => {}
        }
        names
    }

    /// Outline for `glyph_id`, in font units (y up). `None` for empty glyphs
    /// such as space.
    pub fn glyph_outline(&self, glyph_id: u16) -> Option<Path> {
        if let Some(cff) = &self.cff {
            // An OpenType face's glyph ids are its CFF glyph indices.
            return cff.glyph_outline(glyph_id);
        }
        let mut path = Path::with_tolerance(self.units_per_em / 300.0);
        self.append_glyph(glyph_id, &mut path, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0)?;
        if path.subpaths.is_empty() {
            None
        } else {
            Some(path)
        }
    }

    /// Recursive worker shared by simple and composite glyphs. The `a..f`
    /// arguments are the component transform accumulated so far.
    #[allow(clippy::too_many_arguments)]
    fn append_glyph(
        &self,
        glyph_id: u16,
        path: &mut Path,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        e: f64,
        f: f64,
        depth: usize,
    ) -> Option<()> {
        if depth > 5 {
            return Some(()); // cyclic or pathological composite
        }
        let (glyf_offset, _) = *self.tables.get(b"glyf")?;
        let index = glyph_id as usize;
        if index + 1 >= self.loca.len() {
            return Some(());
        }
        let start = glyf_offset + self.loca[index] as usize;
        let end = glyf_offset + self.loca[index + 1] as usize;
        if end <= start || end > self.data.len() {
            return Some(()); // empty glyph (e.g. space)
        }

        let contour_count = read_i16(&self.data, start)?;
        if contour_count >= 0 {
            self.append_simple_glyph(start, contour_count as usize, path, a, b, c, d, e, f)
        } else {
            self.append_composite_glyph(start, end, path, a, b, c, d, e, f, depth)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_simple_glyph(
        &self,
        start: usize,
        contour_count: usize,
        path: &mut Path,
        ma: f64,
        mb: f64,
        mc: f64,
        md: f64,
        me: f64,
        mf: f64,
    ) -> Option<()> {
        let data = &self.data;
        let mut cursor = start + 10;

        let mut end_points = Vec::with_capacity(contour_count);
        for _ in 0..contour_count {
            end_points.push(read_u16(data, cursor)?);
            cursor += 2;
        }
        let point_count = end_points.last().map(|&e| e as usize + 1).unwrap_or(0);
        if point_count == 0 {
            return Some(());
        }

        let instruction_length = read_u16(data, cursor)? as usize;
        cursor += 2 + instruction_length;

        // Flags, run-length encoded via the REPEAT bit.
        let mut flags = Vec::with_capacity(point_count);
        while flags.len() < point_count {
            let flag = *data.get(cursor)?;
            cursor += 1;
            flags.push(flag);
            if flag & 0x08 != 0 {
                let repeats = *data.get(cursor)?;
                cursor += 1;
                for _ in 0..repeats {
                    if flags.len() >= point_count {
                        break;
                    }
                    flags.push(flag);
                }
            }
        }

        // Coordinates are stored as deltas, x's then y's.
        let mut xs = Vec::with_capacity(point_count);
        let mut value = 0i32;
        for &flag in &flags {
            if flag & 0x02 != 0 {
                let delta = *data.get(cursor)? as i32;
                cursor += 1;
                value += if flag & 0x10 != 0 { delta } else { -delta };
            } else if flag & 0x10 == 0 {
                value += read_i16(data, cursor)? as i32;
                cursor += 2;
            }
            xs.push(value);
        }
        let mut ys = Vec::with_capacity(point_count);
        value = 0;
        for &flag in &flags {
            if flag & 0x04 != 0 {
                let delta = *data.get(cursor)? as i32;
                cursor += 1;
                value += if flag & 0x20 != 0 { delta } else { -delta };
            } else if flag & 0x20 == 0 {
                value += read_i16(data, cursor)? as i32;
                cursor += 2;
            }
            ys.push(value);
        }

        let transform = |x: f64, y: f64| (ma * x + mc * y + me, mb * x + md * y + mf);

        let mut first = 0usize;
        for &end in &end_points {
            let last = end as usize;
            if last < first || last >= point_count {
                break;
            }
            emit_contour(
                path,
                &flags[first..=last],
                &xs[first..=last],
                &ys[first..=last],
                &transform,
            );
            first = last + 1;
        }
        Some(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_composite_glyph(
        &self,
        start: usize,
        end: usize,
        path: &mut Path,
        ma: f64,
        mb: f64,
        mc: f64,
        md: f64,
        me: f64,
        mf: f64,
        depth: usize,
    ) -> Option<()> {
        let data = &self.data;
        let mut cursor = start + 10;
        loop {
            if cursor + 4 > end {
                break;
            }
            let flags = read_u16(data, cursor)?;
            let component_id = read_u16(data, cursor + 2)?;
            cursor += 4;

            const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
            const ARGS_ARE_XY_VALUES: u16 = 0x0002;
            const WE_HAVE_A_SCALE: u16 = 0x0008;
            const MORE_COMPONENTS: u16 = 0x0020;
            const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
            const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;

            let (dx, dy) = if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                let a1 = read_i16(data, cursor)? as f64;
                let a2 = read_i16(data, cursor + 2)? as f64;
                cursor += 4;
                (a1, a2)
            } else {
                let a1 = *data.get(cursor)? as i8 as f64;
                let a2 = *data.get(cursor + 1)? as i8 as f64;
                cursor += 2;
                (a1, a2)
            };
            // Point-matching placement is rare; treat it as no offset.
            let (dx, dy) = if flags & ARGS_ARE_XY_VALUES != 0 {
                (dx, dy)
            } else {
                (0.0, 0.0)
            };

            let (sa, sb, sc, sd) = if flags & WE_HAVE_A_SCALE != 0 {
                let s = read_f2dot14(data, cursor)?;
                cursor += 2;
                (s, 0.0, 0.0, s)
            } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                let sx = read_f2dot14(data, cursor)?;
                let sy = read_f2dot14(data, cursor + 2)?;
                cursor += 4;
                (sx, 0.0, 0.0, sy)
            } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                let a = read_f2dot14(data, cursor)?;
                let b = read_f2dot14(data, cursor + 2)?;
                let c = read_f2dot14(data, cursor + 4)?;
                let d = read_f2dot14(data, cursor + 6)?;
                cursor += 8;
                (a, b, c, d)
            } else {
                (1.0, 0.0, 0.0, 1.0)
            };

            // Compose component transform with the parent's.
            self.append_glyph(
                component_id,
                path,
                sa * ma + sb * mc,
                sa * mb + sb * md,
                sc * ma + sd * mc,
                sc * mb + sd * md,
                dx * ma + dy * mc + me,
                dx * mb + dy * md + mf,
                depth + 1,
            )?;

            if flags & MORE_COMPONENTS == 0 {
                break;
            }
        }
        Some(())
    }

    fn load_cmap(&mut self) {
        let Some(&(cmap_offset, _)) = self.tables.get(b"cmap") else {
            return;
        };
        let Some(table_count) = read_u16(&self.data, cmap_offset + 2) else {
            return;
        };

        // Preference order: (3,10) full Unicode, (3,1) BMP, (0,x) Unicode,
        // then (3,0) symbol, then (1,0) Mac Roman.
        let mut best: Option<(u32, usize, bool)> = None;
        for i in 0..table_count as usize {
            let record = cmap_offset + 4 + i * 8;
            let Some(platform) = read_u16(&self.data, record) else {
                continue;
            };
            let Some(encoding) = read_u16(&self.data, record + 2) else {
                continue;
            };
            let Some(offset) = read_u32(&self.data, record + 4) else {
                continue;
            };
            let (rank, symbolic) = match (platform, encoding) {
                (3, 10) => (5, false),
                (3, 1) => (4, false),
                (0, _) => (3, false),
                (3, 0) => (2, true),
                (1, 0) => (1, false),
                _ => (0, false),
            };
            if rank == 0 {
                continue;
            }
            if best.map(|(r, _, _)| rank > r).unwrap_or(true) {
                best = Some((rank, cmap_offset + offset as usize, symbolic));
            }
        }

        if let Some((_, subtable, symbolic)) = best {
            self.symbolic_cmap = symbolic;
            self.parse_cmap_subtable(subtable);
        }
    }

    fn parse_cmap_subtable(&mut self, offset: usize) {
        let data = &self.data;
        let Some(format) = read_u16(data, offset) else {
            return;
        };
        let mut cmap = HashMap::new();
        match format {
            0 => {
                for code in 0..256usize {
                    if let Some(&gid) = data.get(offset + 6 + code) {
                        if gid != 0 {
                            cmap.insert(code as u32, gid as u16);
                        }
                    }
                }
            }
            4 => {
                let Some(seg_x2) = read_u16(data, offset + 6) else {
                    return;
                };
                let segments = seg_x2 as usize / 2;
                let ends = offset + 14;
                let starts = ends + seg_x2 as usize + 2;
                let deltas = starts + seg_x2 as usize;
                let ranges = deltas + seg_x2 as usize;
                for s in 0..segments {
                    let (Some(end), Some(start), Some(delta), Some(range_offset)) = (
                        read_u16(data, ends + s * 2),
                        read_u16(data, starts + s * 2),
                        read_u16(data, deltas + s * 2),
                        read_u16(data, ranges + s * 2),
                    ) else {
                        continue;
                    };
                    if start > end {
                        continue;
                    }
                    for code in start..=end {
                        if code == 0xFFFF {
                            continue;
                        }
                        let gid = if range_offset == 0 {
                            code.wrapping_add(delta)
                        } else {
                            let index = ranges
                                + s * 2
                                + range_offset as usize
                                + (code - start) as usize * 2;
                            match read_u16(data, index) {
                                Some(0) | None => continue,
                                Some(g) => g.wrapping_add(delta),
                            }
                        };
                        if gid != 0 {
                            cmap.insert(code as u32, gid);
                        }
                    }
                }
            }
            6 => {
                let (Some(first), Some(count)) =
                    (read_u16(data, offset + 6), read_u16(data, offset + 8))
                else {
                    return;
                };
                for i in 0..count as usize {
                    if let Some(gid) = read_u16(data, offset + 10 + i * 2) {
                        if gid != 0 {
                            cmap.insert(first as u32 + i as u32, gid);
                        }
                    }
                }
            }
            12 => {
                let Some(groups) = read_u32(data, offset + 12) else {
                    return;
                };
                for g in 0..groups.min(100_000) as usize {
                    let record = offset + 16 + g * 12;
                    let (Some(start), Some(end), Some(start_gid)) = (
                        read_u32(data, record),
                        read_u32(data, record + 4),
                        read_u32(data, record + 8),
                    ) else {
                        continue;
                    };
                    if end < start || end - start > 0x10_000 {
                        continue;
                    }
                    for code in start..=end {
                        cmap.insert(code, (start_gid + (code - start)) as u16);
                    }
                }
            }
            _ => {}
        }
        self.cmap = cmap;
    }
}

/// Turn one TrueType contour into path segments.
///
/// TrueType contours are quadratic B-splines: consecutive off-curve points
/// imply an on-curve point at their midpoint.
fn emit_contour(
    path: &mut Path,
    flags: &[u8],
    xs: &[i32],
    ys: &[i32],
    transform: &impl Fn(f64, f64) -> (f64, f64),
) {
    let n = flags.len();
    if n == 0 {
        return;
    }
    let on_curve = |i: usize| flags[i % n] & 0x01 != 0;
    let point = |i: usize| (xs[i % n] as f64, ys[i % n] as f64);
    let midpoint = |i: usize, j: usize| {
        let (x0, y0) = point(i);
        let (x1, y1) = point(j);
        ((x0 + x1) / 2.0, (y0 + y1) / 2.0)
    };

    // Find a starting on-curve point, synthesising one if the contour is all
    // off-curve (legal, and produced by some subsetters).
    let start_index = (0..n).find(|&i| on_curve(i));
    let (start_x, start_y) = match start_index {
        Some(i) => point(i),
        None => midpoint(0, 1),
    };
    let (sx, sy) = transform(start_x, start_y);
    path.move_to(sx, sy);

    let begin = start_index.map(|i| i + 1).unwrap_or(1);
    let mut pending_control: Option<(f64, f64)> = None;

    for step in 0..n {
        let i = begin + step;
        let (px, py) = point(i);
        if on_curve(i) {
            match pending_control.take() {
                Some((cx, cy)) => {
                    let (tcx, tcy) = transform(cx, cy);
                    let (tx, ty) = transform(px, py);
                    path.quad_to(tcx, tcy, tx, ty);
                }
                None => {
                    let (tx, ty) = transform(px, py);
                    path.line_to(tx, ty);
                }
            }
        } else {
            if let Some((cx, cy)) = pending_control {
                // Two off-curve points in a row: implied on-curve midpoint.
                let mx = (cx + px) / 2.0;
                let my = (cy + py) / 2.0;
                let (tcx, tcy) = transform(cx, cy);
                let (tmx, tmy) = transform(mx, my);
                path.quad_to(tcx, tcy, tmx, tmy);
            }
            pending_control = Some((px, py));
        }
    }

    // Close back onto the start point, through any dangling control point.
    if let Some((cx, cy)) = pending_control {
        let (tcx, tcy) = transform(cx, cy);
        path.quad_to(tcx, tcy, sx, sy);
    }
    path.close();
}

fn read_loca(
    data: &[u8],
    offset: usize,
    length: usize,
    num_glyphs: u16,
    format: i16,
) -> Option<Vec<u32>> {
    let count = num_glyphs as usize + 1;
    let mut loca = Vec::with_capacity(count);
    if format == 0 {
        if length < count * 2 {
            return None;
        }
        for i in 0..count {
            loca.push(read_u16(data, offset + i * 2)? as u32 * 2);
        }
    } else {
        if length < count * 4 {
            return None;
        }
        for i in 0..count {
            loca.push(read_u32(data, offset + i * 4)?);
        }
    }
    Some(loca)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *data.get(offset)?,
        *data.get(offset + 1)?,
    ]))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|v| v as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *data.get(offset)?,
        *data.get(offset + 1)?,
        *data.get(offset + 2)?,
        *data.get(offset + 3)?,
    ]))
}

/// F2Dot14: signed 2.14 fixed point.
fn read_f2dot14(data: &[u8], offset: usize) -> Option<f64> {
    read_i16(data, offset).map(|v| v as f64 / 16384.0)
}

/// The standard Macintosh glyph order, which `post` format 1.0 assigns to
/// glyphs 0..258 and format 2.0 indices below 258 refer to.
const MAC_GLYPH_NAMES: [&str; 258] = [
    ".notdef",
    ".null",
    "nonmarkingreturn",
    "space",
    "exclam",
    "quotedbl",
    "numbersign",
    "dollar",
    "percent",
    "ampersand",
    "quotesingle",
    "parenleft",
    "parenright",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "less",
    "equal",
    "greater",
    "question",
    "at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "bracketleft",
    "backslash",
    "bracketright",
    "asciicircum",
    "underscore",
    "grave",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "braceleft",
    "bar",
    "braceright",
    "asciitilde",
    "Adieresis",
    "Aring",
    "Ccedilla",
    "Eacute",
    "Ntilde",
    "Odieresis",
    "Udieresis",
    "aacute",
    "agrave",
    "acircumflex",
    "adieresis",
    "atilde",
    "aring",
    "ccedilla",
    "eacute",
    "egrave",
    "ecircumflex",
    "edieresis",
    "iacute",
    "igrave",
    "icircumflex",
    "idieresis",
    "ntilde",
    "oacute",
    "ograve",
    "ocircumflex",
    "odieresis",
    "otilde",
    "uacute",
    "ugrave",
    "ucircumflex",
    "udieresis",
    "dagger",
    "degree",
    "cent",
    "sterling",
    "section",
    "bullet",
    "paragraph",
    "germandbls",
    "registered",
    "copyright",
    "trademark",
    "acute",
    "dieresis",
    "notequal",
    "AE",
    "Oslash",
    "infinity",
    "plusminus",
    "lessequal",
    "greaterequal",
    "yen",
    "mu",
    "partialdiff",
    "summation",
    "product",
    "pi",
    "integral",
    "ordfeminine",
    "ordmasculine",
    "Omega",
    "ae",
    "oslash",
    "questiondown",
    "exclamdown",
    "logicalnot",
    "radical",
    "florin",
    "approxequal",
    "Delta",
    "guillemotleft",
    "guillemotright",
    "ellipsis",
    "nonbreakingspace",
    "Agrave",
    "Atilde",
    "Otilde",
    "OE",
    "oe",
    "endash",
    "emdash",
    "quotedblleft",
    "quotedblright",
    "quoteleft",
    "quoteright",
    "divide",
    "lozenge",
    "ydieresis",
    "Ydieresis",
    "fraction",
    "currency",
    "guilsinglleft",
    "guilsinglright",
    "fi",
    "fl",
    "daggerdbl",
    "periodcentered",
    "quotesinglbase",
    "quotedblbase",
    "perthousand",
    "Acircumflex",
    "Ecircumflex",
    "Aacute",
    "Edieresis",
    "Egrave",
    "Iacute",
    "Icircumflex",
    "Idieresis",
    "Igrave",
    "Oacute",
    "Ocircumflex",
    "apple",
    "Ograve",
    "Uacute",
    "Ucircumflex",
    "Ugrave",
    "dotlessi",
    "circumflex",
    "tilde",
    "macron",
    "breve",
    "dotaccent",
    "ring",
    "cedilla",
    "hungarumlaut",
    "ogonek",
    "caron",
    "Lslash",
    "lslash",
    "Scaron",
    "scaron",
    "Zcaron",
    "zcaron",
    "brokenbar",
    "Eth",
    "eth",
    "Yacute",
    "yacute",
    "Thorn",
    "thorn",
    "minus",
    "multiply",
    "onesuperior",
    "twosuperior",
    "threesuperior",
    "onehalf",
    "onequarter",
    "threequarters",
    "franc",
    "Gbreve",
    "gbreve",
    "Idotaccent",
    "Scedilla",
    "scedilla",
    "Cacute",
    "cacute",
    "Ccaron",
    "ccaron",
    "dcroat",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_glyph_names_are_the_standard_258_in_order() {
        // Spot checks across the table; one shifted entry would misname every
        // glyph after it in a format 1.0 or 2.0 `post` table.
        assert_eq!(MAC_GLYPH_NAMES[0], ".notdef");
        assert_eq!(MAC_GLYPH_NAMES[3], "space");
        assert_eq!(MAC_GLYPH_NAMES[36], "A");
        assert_eq!(MAC_GLYPH_NAMES[68], "a");
        assert_eq!(MAC_GLYPH_NAMES[146], "infinity");
        assert_eq!(MAC_GLYPH_NAMES[172], "nonbreakingspace");
        assert_eq!(MAC_GLYPH_NAMES[210], "apple");
        assert_eq!(MAC_GLYPH_NAMES[257], "dcroat");
    }

    /// A minimal sfnt carrying just `post`, enough for name lookups.
    fn sfnt_with_post(post: &[u8], num_glyphs: u16) -> TrueTypeFont {
        let mut tables = HashMap::new();
        let mut data = vec![0u8; 12];
        let offset = data.len();
        data.extend_from_slice(post);
        tables.insert(*b"post", (offset, post.len()));
        TrueTypeFont {
            data,
            tables,
            units_per_em: 1000.0,
            num_glyphs,
            loca: Vec::new(),
            cmap: HashMap::new(),
            symbolic_cmap: false,
            post_names: std::sync::OnceLock::new(),
            cff: None,
        }
    }

    #[test]
    fn post_format_two_resolves_standard_and_custom_names() {
        // Header (32 bytes), 3 glyphs: .notdef, "exclam" (standard 4), and a
        // custom name at index 258.
        let mut post = vec![0, 2, 0, 0];
        post.extend_from_slice(&[0; 28]);
        post.extend_from_slice(&3u16.to_be_bytes());
        for index in [0u16, 4, 258] {
            post.extend_from_slice(&index.to_be_bytes());
        }
        post.push(4);
        post.extend_from_slice(b"ttaa");
        let font = sfnt_with_post(&post, 3);
        assert_eq!(font.gid_for_name("exclam"), Some(1));
        assert_eq!(font.gid_for_name("ttaa"), Some(2));
        assert_eq!(font.gid_for_name("nonesuch"), None);
    }

    #[test]
    fn post_format_one_uses_the_mac_order() {
        let mut post = vec![0, 1, 0, 0];
        post.extend_from_slice(&[0; 28]);
        let font = sfnt_with_post(&post, 258);
        assert_eq!(font.gid_for_name("A"), Some(36));
        assert_eq!(font.gid_for_name("infinity"), Some(146));
    }

    #[test]
    fn rejects_non_sfnt_data() {
        assert!(TrueTypeFont::parse(b"not a font at all".to_vec()).is_none());
        assert!(TrueTypeFont::parse(b"OTTO\0\0\0\0\0\0\0\0".to_vec()).is_none());
    }

    #[test]
    fn f2dot14_reads_signed_fixed_point() {
        assert_eq!(read_f2dot14(&[0x40, 0x00], 0), Some(1.0));
        assert_eq!(read_f2dot14(&[0xC0, 0x00], 0), Some(-1.0));
        assert_eq!(read_f2dot14(&[0x00, 0x00], 0), Some(0.0));
    }

    #[test]
    fn contour_of_straight_lines_becomes_a_polygon() {
        let mut path = Path::new();
        // A closed triangle, all points on-curve.
        emit_contour(
            &mut path,
            &[0x01, 0x01, 0x01],
            &[0, 100, 50],
            &[0, 0, 100],
            &|x, y| (x, y),
        );
        assert_eq!(path.subpaths.len(), 1);
        assert!(path.subpaths[0].closed);
        assert_eq!(path.bounds(), Some((0.0, 0.0, 100.0, 100.0)));
    }

    #[test]
    fn off_curve_points_produce_curves() {
        let mut path = Path::new();
        // on, off, on — one quadratic arc.
        emit_contour(
            &mut path,
            &[0x01, 0x00, 0x01],
            &[0, 50, 100],
            &[0, 100, 0],
            &|x, y| (x, y),
        );
        let points = &path.subpaths[0].points;
        assert!(
            points.len() > 4,
            "curve should be flattened into several points, got {}",
            points.len()
        );
        let peak = points.iter().map(|p| p.y).fold(f64::MIN, f64::max);
        // A quadratic's apex sits halfway to the control point.
        assert!((peak - 50.0).abs() < 2.0, "peak was {peak}");
    }

    #[test]
    fn all_off_curve_contour_synthesises_a_start_point() {
        let mut path = Path::new();
        emit_contour(
            &mut path,
            &[0x00, 0x00, 0x00, 0x00],
            &[0, 100, 100, 0],
            &[0, 0, 100, 100],
            &|x, y| (x, y),
        );
        assert_eq!(path.subpaths.len(), 1);
        assert!(!path.subpaths[0].points.is_empty());
    }
}
