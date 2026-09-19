//! Minimal TrueType/OpenType parser: enough to turn a glyph id into an
//! outline, and a character code into a glyph id.
//!
//! Covers `head`, `maxp`, `loca`, `glyf` (simple *and* composite glyphs) and
//! `cmap` formats 0, 4, 6 and 12 — which is what PDF font subsets embedded as
//! `/FontFile2` actually use. CFF/Type1C outlines (`/FontFile3`) are a
//! different charstring format and are not handled here; see
//! [`super::GlyphSource`] for how that is reported.

use std::collections::HashMap;

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
}

impl TrueTypeFont {
    pub fn parse(data: Vec<u8>) -> Option<TrueTypeFont> {
        if data.len() < 12 {
            return None;
        }
        // A TrueType Collection points at its first font.
        let base = if &data[0..4] == b"ttcf" {
            read_u32(&data, 12)? as usize
        } else {
            0
        };
        let tag = read_u32(&data, base)?;
        // 0x00010000 = TrueType outlines, 'true' = legacy Apple.
        // 'OTTO' means CFF outlines, which this parser cannot read.
        if tag != 0x0001_0000 && tag != 0x7472_7565 {
            return None;
        }

        let table_count = read_u16(&data, base + 4)? as usize;
        let mut tables = HashMap::with_capacity(table_count);
        for i in 0..table_count {
            let record = base + 12 + i * 16;
            if record + 16 > data.len() {
                break;
            }
            let mut name = [0u8; 4];
            name.copy_from_slice(&data[record..record + 4]);
            let offset = read_u32(&data, record + 8)? as usize;
            let length = read_u32(&data, record + 12)? as usize;
            if offset <= data.len() {
                tables.insert(name, (offset, length.min(data.len() - offset)));
            }
        }

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

        let mut font = TrueTypeFont {
            units_per_em: if units_per_em > 0.0 { units_per_em } else { 1000.0 },
            num_glyphs,
            loca,
            cmap: HashMap::new(),
            symbolic_cmap: false,
            tables,
            data,
        };
        font.load_cmap();
        Some(font)
    }

    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
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

    pub fn has_outlines(&self) -> bool {
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

    /// Outline for `glyph_id`, in font units (y up). `None` for empty glyphs
    /// such as space.
    pub fn glyph_outline(&self, glyph_id: u16) -> Option<Path> {
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
            self.append_simple_glyph(
                start,
                contour_count as usize,
                path,
                a,
                b,
                c,
                d,
                e,
                f,
            )
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

#[cfg(test)]
mod tests {
    use super::*;

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
