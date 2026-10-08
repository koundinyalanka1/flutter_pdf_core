//! Invisible text layers: recognized words written into a page, exactly over
//! the glyphs they were read from, so text extraction, selection, search and
//! AI export (here and in other viewers) find text on a scanned page.
//!
//! One font serves the whole document: a Type0 font over the glyphless
//! TrueType in [`crate::glyphless`], with CIDs numbered per distinct character
//! and a ToUnicode map naming only the characters used. Text is drawn in
//! rendering mode 3 (invisible), so the page renders exactly as before.
//!
//! Geometry: the font box spans 0.8 em above the baseline and 0.2 em below,
//! every glyph advances half an em, and each line is set at a font size
//! equal to its height. Then:
//!
//! * a line that runs horizontally in PDF user space gets one text run per
//!   word, stretched with `Tz` to the word's exact width, and an explicit
//!   space run across each gap, so extractors never have to guess where words
//!   end and the selection boxes match the ink;
//! * a line at an angle in user space (a page with /Rotate, a skewed scan) is
//!   one `TJ` array with every glyph placed by kerning, because Apple's
//!   PDFKit only treats a rotated line as one line when a single operator
//!   draws it. Its cell width is the widest word's pitch, so glyphs overlap
//!   rather than leave gaps that would read as spaces.
//!
//! The original content is wrapped in `q`/`Q`, with any `q` it left
//! unmatched closed first, so its graphics state cannot leak into the layer.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::filter::flate_encode;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_ops::page_tree::effective_page_dict;
use pdf_text::content_stream::parse_content;
use pdf_text::layout::page_geometry;
use pdf_text::text_state::Matrix;

/// Glyph advance, in em.
const ADVANCE: f64 = 0.5;
/// Font box below the baseline, in em (FontDescriptor /Descent).
const DESCENT: f64 = 0.2;
/// Lines within this angle of user-space horizontal count as horizontal.
const LEVEL: f64 = 0.05;
/// More distinct characters than this do not fit one Identity-H font.
const MAX_CHARACTERS: usize = 65_000;

/// A word to write, with its corners in displayed page points (top-left
/// origin, y down: the space of `pdf_page_text_layout_json`): top-left,
/// top-right, bottom-right, bottom-left.
#[derive(Clone, Debug, PartialEq)]
pub struct WordBox {
    pub text: String,
    pub quad: [[f64; 2]; 4],
}

/// The words for one page (0-based), line by line in reading order.
#[derive(Clone, Debug, PartialEq)]
pub struct PageWords {
    pub page_index: usize,
    pub lines: Vec<Vec<WordBox>>,
}

/// Write a text layer onto each page that has words. Returns how many pages
/// received one.
pub fn add_text_layers(doc: &mut PdfDocument, pages: &[PageWords]) -> Result<usize> {
    let characters: BTreeSet<char> = pages
        .iter()
        .flat_map(|p| p.lines.iter().flatten())
        .flat_map(|w| w.text.chars())
        .chain([' '])
        .collect();
    if characters.len() > MAX_CHARACTERS {
        return Err(PdfError::Structure(
            "too many distinct characters for one text layer".into(),
        ));
    }
    let characters: Vec<char> = characters.into_iter().collect();
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let pages: Vec<&PageWords> = pages
        .iter()
        .filter(|p| p.lines.iter().flatten().any(|w| !w.text.trim().is_empty()))
        .collect();
    if pages.is_empty() {
        return Ok(0);
    }
    let font = write_font(doc, &characters);
    for page in &pages {
        let &page_id = page_ids
            .get(page.page_index)
            .ok_or(PdfError::PageIndex(page.page_index))?;
        write_page(doc, page_id, &page.lines, font, &characters)?;
    }
    Ok(pages.len())
}

/// The layer's content operators on their own, plus what they need, for a
/// page of the given geometry. Used to measure a layer without writing it.
pub(crate) fn layer_content(
    lines: &[Vec<WordBox>],
    font_name: &str,
    to_user: &Matrix,
    characters: &[char],
) -> String {
    let cid = |ch: char| characters.binary_search(&ch).map(|i| i + 1).unwrap_or(0);
    let hex = |text: &str| {
        text.chars().fold(String::new(), |mut s, ch| {
            let _ = write!(s, "{:04X}", cid(ch));
            s
        })
    };
    let mut ops = String::from("BT\n3 Tr\n");
    for line in lines {
        let words: Vec<&WordBox> = line.iter().filter(|w| !w.text.trim().is_empty()).collect();
        let Some(first) = words.first() else {
            continue;
        };
        // The line's frame in displayed space: along its baseline and down.
        let mut along = (0.0, 0.0);
        for w in &words {
            let [tl, tr, br, bl] = w.quad;
            along.0 += tr[0] - tl[0] + br[0] - bl[0];
            along.1 += tr[1] - tl[1] + br[1] - bl[1];
        }
        let length = along.0.hypot(along.1);
        let along = if length > 1e-9 {
            (along.0 / length, along.1 / length)
        } else {
            (1.0, 0.0)
        };
        let down = (-along.1, along.0);
        let origin = first.quad[0];
        let project = |p: [f64; 2]| {
            let (dx, dy) = (p[0] - origin[0], p[1] - origin[1]);
            (dx * along.0 + dy * along.1, dx * down.0 + dy * down.1)
        };
        let (mut top, mut bottom) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut spans = Vec::with_capacity(words.len());
        for w in &words {
            let (mut start, mut end) = (f64::INFINITY, f64::NEG_INFINITY);
            for corner in w.quad {
                let (s, t) = project(corner);
                (start, end) = (start.min(s), end.max(s));
                (top, bottom) = (top.min(t), bottom.max(t));
            }
            spans.push((w.text.trim(), start, end.max(start + 0.01)));
        }
        let size = (bottom - top).max(0.5);
        let baseline = bottom - DESCENT * size;
        let mut runs: Vec<(&str, f64, f64)> = Vec::with_capacity(spans.len() * 2);
        for (i, &(text, start, end)) in spans.iter().enumerate() {
            runs.push((text, start, end));
            if let Some(&(_, next, _)) = spans.get(i + 1) {
                if next > end + 0.01 {
                    runs.push((" ", end, next));
                }
            }
        }
        let point = |s: f64| {
            let (x, y) = (
                origin[0] + s * along.0 + baseline * down.0,
                origin[1] + s * along.1 + baseline * down.1,
            );
            to_user.transform_point(x, y)
        };
        let unit = |v: (f64, f64)| {
            let l = v.0.hypot(v.1).max(1e-12);
            (v.0 / l, v.1 / l)
        };
        let (ux, uy) = unit(to_user.transform_vector(along.0, along.1));
        let (vx, vy) = unit(to_user.transform_vector(-down.0, -down.1));
        let frame = |s: f64| {
            let (x, y) = point(s);
            format!("{ux:.6} {uy:.6} {vx:.6} {vy:.6} {x:.4} {y:.4} Tm")
        };
        let _ = writeln!(ops, "/{font_name} {size:.4} Tf");

        if uy.abs() < LEVEL && ux > 0.0 {
            for (text, start, end) in runs {
                let n = text.chars().count() as f64;
                let tz = 100.0 * (end - start) / (n * ADVANCE * size);
                let _ = writeln!(ops, "{tz:.4} Tz {} <{}> Tj", frame(start), hex(text));
            }
            continue;
        }

        // Rotated: one TJ for the whole line.
        let cell = runs
            .iter()
            .filter(|r| r.0 != " " && r.0.chars().count() >= 2)
            .map(|r| (r.2 - r.1) / r.0.chars().count() as f64)
            .fold(f64::NAN, f64::max);
        let cell = if cell.is_finite() {
            cell
        } else {
            let total: f64 = runs.iter().map(|r| r.2 - r.1).sum();
            total / runs.iter().map(|r| r.0.chars().count()).sum::<usize>() as f64
        };
        let tz = 100.0 * cell / (ADVANCE * size);
        let unit_width = size * tz / 100.0 / 1000.0; // points per TJ unit
        let mut array = String::new();
        let mut pen = runs[0].1;
        for &(text, start, end) in &runs {
            let n = text.chars().count();
            let width = end - start;
            let pitch = if n >= 2 && width >= cell {
                (width - cell) / (n - 1) as f64
            } else {
                width / n as f64
            };
            for (k, ch) in text.chars().enumerate() {
                let target = start + pitch * k as f64;
                if (target - pen).abs() > 1e-4 {
                    let _ = write!(array, " {:.3}", -(target - pen) / unit_width);
                }
                let _ = write!(array, " <{:04X}>", cid(ch));
                pen = target + cell;
            }
        }
        let _ = writeln!(ops, "{tz:.4} Tz {} [{array} ] TJ", frame(runs[0].1));
    }
    ops.push_str("ET\n");
    ops
}

fn write_page(
    doc: &mut PdfDocument,
    page_id: ObjectId,
    lines: &[Vec<WordBox>],
    font: ObjectId,
    characters: &[char],
) -> Result<()> {
    let geometry = page_geometry(doc, page_id)?;
    let to_user = geometry
        .transform
        .invert()
        .ok_or_else(|| PdfError::Structure("page transform is not invertible".into()))?;
    let effective = effective_page_dict(doc, page_id)?;

    let mut resources = effective
        .get("Resources")
        .and_then(|r| doc.resolve_dict(r).cloned())
        .unwrap_or_default();
    let mut fonts = resources
        .get("Font")
        .and_then(|f| doc.resolve_dict(f).cloned())
        .unwrap_or_default();
    let font_name = (0..)
        .map(|i| format!("OCR{i}"))
        .find(|name| !fonts.contains_key(name))
        .expect("an unused name");
    fonts.insert(font_name.clone(), PdfObject::Reference(font));
    resources.insert("Font".into(), PdfObject::Dictionary(fonts));

    let mut kept = Vec::new();
    match effective.get("Contents").map(|c| doc.resolve_value(c)) {
        Some(PdfObject::Array(items)) => kept.extend(items),
        Some(PdfObject::Stream(_)) => kept.push(effective["Contents"].clone()),
        _ => {}
    }
    // Close whatever `q` the original content left open, so the layer starts
    // from the page's own coordinate system.
    let mut depth = 0usize;
    for item in &kept {
        if let PdfObject::Stream(stream) = doc.resolve_value(item) {
            for op in
                parse_content(&doc.stream_data(&stream).unwrap_or_default()).unwrap_or_default()
            {
                match op.operator.as_str() {
                    "q" => depth += 1,
                    "Q" => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
    }
    let layer = layer_content(lines, &font_name, &to_user, characters);
    let suffix = format!("{}Q\n{layer}", "Q\n".repeat(depth));
    let prefix = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        b"q\n".to_vec(),
    )));
    let suffix = doc.add_object(flate_stream(Dictionary::new(), suffix.as_bytes()));
    let mut contents = vec![PdfObject::Reference(prefix)];
    contents.extend(kept);
    contents.push(PdfObject::Reference(suffix));

    let mut page = doc
        .resolve(page_id)
        .and_then(PdfObject::as_dict)
        .cloned()
        .ok_or_else(|| PdfError::Structure("page object missing".into()))?;
    page.insert("Resources".into(), PdfObject::Dictionary(resources));
    page.insert("Contents".into(), PdfObject::Array(contents));
    doc.set_object(page_id, PdfObject::Dictionary(page));
    Ok(())
}

fn flate_stream(mut dict: Dictionary, data: &[u8]) -> PdfObject {
    dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
    PdfObject::Stream(PdfStream::new(dict, flate_encode(data)))
}

fn name(n: &str) -> PdfObject {
    PdfObject::Name(n.into())
}

/// The Type0 font, its glyphless program and its ToUnicode map. Character
/// `characters[i]` is CID `i + 1`.
pub(crate) fn write_font(doc: &mut PdfDocument, characters: &[char]) -> ObjectId {
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let entries: Vec<(usize, char)> = characters
        .iter()
        .enumerate()
        .map(|(i, &c)| (i + 1, c))
        .collect();
    for chunk in entries.chunks(100) {
        let _ = writeln!(cmap, "{} beginbfchar", chunk.len());
        for &(cid, ch) in chunk {
            let mut units = [0u16; 2];
            let target: String = ch
                .encode_utf16(&mut units)
                .iter()
                .map(|u| format!("{u:04X}"))
                .collect();
            let _ = writeln!(cmap, "<{cid:04X}> <{target}>");
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    let to_unicode = doc.add_object(flate_stream(Dictionary::new(), cmap.as_bytes()));

    let program = crate::glyphless::font_program();
    let program = doc.add_object(flate_stream(
        Dictionary::from([("Length1".into(), PdfObject::Integer(program.len() as i64))]),
        &program,
    ));
    let descriptor = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), name("FontDescriptor")),
        ("FontName".into(), name("GlyphLessFont")),
        ("Flags".into(), PdfObject::Integer(5)),
        (
            "FontBBox".into(),
            PdfObject::Array([0, -200, 500, 800].map(PdfObject::Integer).to_vec()),
        ),
        ("ItalicAngle".into(), PdfObject::Integer(0)),
        ("Ascent".into(), PdfObject::Integer(800)),
        ("Descent".into(), PdfObject::Integer(-200)),
        ("CapHeight".into(), PdfObject::Integer(800)),
        ("StemV".into(), PdfObject::Integer(80)),
        ("FontFile2".into(), PdfObject::Reference(program)),
    ])));
    // Every CID but 0 draws glyph 1, which is empty.
    let gid_map: Vec<u8> = (0..=characters.len())
        .flat_map(|cid| u16::from(cid != 0).to_be_bytes())
        .collect();
    let gid_map = doc.add_object(flate_stream(Dictionary::new(), &gid_map));
    let cid_font = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), name("Font")),
        ("Subtype".into(), name("CIDFontType2")),
        ("BaseFont".into(), name("GlyphLessFont")),
        (
            "CIDSystemInfo".into(),
            PdfObject::Dictionary(Dictionary::from([
                (
                    "Registry".into(),
                    PdfObject::LiteralString(b"Adobe".to_vec()),
                ),
                (
                    "Ordering".into(),
                    PdfObject::LiteralString(b"Identity".to_vec()),
                ),
                ("Supplement".into(), PdfObject::Integer(0)),
            ])),
        ),
        ("FontDescriptor".into(), PdfObject::Reference(descriptor)),
        ("DW".into(), PdfObject::Integer((ADVANCE * 1000.0) as i64)),
        ("CIDToGIDMap".into(), PdfObject::Reference(gid_map)),
    ])));
    doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), name("Font")),
        ("Subtype".into(), name("Type0")),
        ("BaseFont".into(), name("GlyphLessFont")),
        ("Encoding".into(), name("Identity-H")),
        (
            "DescendantFonts".into(),
            PdfObject::Array(vec![PdfObject::Reference(cid_font)]),
        ),
        ("ToUnicode".into(), PdfObject::Reference(to_unicode)),
    ])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_render::{render_page, RenderOptions, RenderSize};
    use pdf_text::layout::{extract_page_layout, PageTextLayout};

    fn page_doc(rotate: i64, crop: Option<[i64; 4]>) -> PdfDocument {
        let mut doc =
            PdfDocument::from_bytes(include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert("Rotate".into(), PdfObject::Integer(rotate));
        if let Some(c) = crop {
            page.insert(
                "CropBox".into(),
                PdfObject::Array(c.map(PdfObject::Integer).to_vec()),
            );
        }
        // An unbalanced `q` with a transform the layer must not inherit.
        let content = doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::new(),
            b"q 2 0 0 2 0 0 cm 0 0 10 10 re f".to_vec(),
        )));
        page.insert("Contents".into(), PdfObject::Reference(content));
        doc.set_object(id, PdfObject::Dictionary(page));
        doc
    }

    fn word(text: &str, l: f64, t: f64, r: f64, b: f64) -> WordBox {
        WordBox {
            text: text.into(),
            quad: [[l, t], [r, t], [r, b], [l, b]],
        }
    }

    /// Union of the glyph boxes of `needle`'s first occurrence after `from`.
    fn box_of(layout: &PageTextLayout, needle: &str, from: &mut usize) -> [f64; 4] {
        let text: Vec<u16> = layout.text.encode_utf16().collect();
        let n: Vec<u16> = needle.encode_utf16().collect();
        let at = (*from..=text.len() - n.len())
            .find(|&i| text[i..i + n.len()] == n[..])
            .expect(needle);
        *from = at + n.len();
        layout
            .glyphs
            .iter()
            .filter(|g| g.start >= at && g.end <= at + n.len())
            .fold(
                [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ],
                |b, g| {
                    [
                        b[0].min(g.bounds[0]),
                        b[1].min(g.bounds[1]),
                        b[2].max(g.bounds[2]),
                        b[3].max(g.bounds[3]),
                    ]
                },
            )
    }

    #[test]
    fn layers_render_invisibly_and_reproduce_word_boxes_for_every_rotation() {
        let lines = vec![
            vec![
                word("Hello", 20.0, 30.0, 70.0, 45.0),
                word("wörld", 78.0, 30.0, 130.0, 45.0),
            ],
            vec![
                word("日本語", 20.0, 50.0, 65.0, 65.0),
                word("😀", 72.0, 50.0, 86.0, 65.0),
            ],
        ];
        for rotate in [0, 90, 180, 270] {
            let original = page_doc(rotate, Some([10, 20, 190, 180]));
            let mut doc = original.clone();
            let pages = [PageWords {
                page_index: 0,
                lines: lines.clone(),
            }];
            assert_eq!(add_text_layers(&mut doc, &pages).unwrap(), 1);

            let options = RenderOptions {
                size: RenderSize::Scale(2.0),
                ..Default::default()
            };
            let (before, after) = (
                render_page(&original, 0, options).unwrap(),
                render_page(&doc, 0, options).unwrap(),
            );
            assert!(
                before.pixels == after.pixels,
                "rotate {rotate}: the layer changed the rendering"
            );
            assert_eq!(before.warnings, after.warnings);

            let doc = PdfDocument::from_bytes(&doc.to_bytes().unwrap()).unwrap();
            let layout = extract_page_layout(&doc, 0).unwrap();
            assert_eq!(layout.text, "Hello wörld\n日本語 😀", "rotate {rotate}");
            assert_eq!(
                pdf_text::extractor::extract_page_text(&doc, 0).unwrap(),
                layout.text
            );
            let mut from = 0;
            for line in &lines {
                let (top, bottom) = (line[0].quad[0][1], line[0].quad[3][1]);
                for w in line {
                    // Exact, except that on a rotated line a one-glyph word
                    // takes the line's common cell width.
                    let tolerance = if rotate != 0 && w.text.chars().count() == 1 {
                        1.5
                    } else {
                        1e-3
                    };
                    let b = box_of(&layout, &w.text, &mut from);
                    let expected = [w.quad[0][0], top, w.quad[1][0], bottom];
                    for (got, want) in b.iter().zip(expected) {
                        assert!(
                            (got - want).abs() <= tolerance,
                            "rotate {rotate} {}: {b:?} vs {expected:?}",
                            w.text
                        );
                    }
                }
            }
        }
    }
}
