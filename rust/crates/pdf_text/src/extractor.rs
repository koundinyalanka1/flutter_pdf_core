//! Milestone 8 (part 4): text extraction.
//!
//! Walks each page's content stream(s) with a faithful text-positioning
//! model and heuristics for word spacing and line breaks. Form XObjects are
//! followed (with a depth limit); images are skipped, apart from measuring
//! how much of the page they cover for [`page_text_stats`].

use std::collections::HashMap;

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use serde::Serialize;

use crate::content_stream::{parse_content, Operation};
use crate::font::{load_font, Font};
use crate::layout::{
    font_vertical_metrics, page_box, page_geometry, transformed_bounds, LayoutFontLoader,
    LayoutFontMetrics, PageGeometry, PageTextLayout, TextGlyph,
};
use crate::text_state::{Matrix, TextObject, TextState};

/// What kind of text a page carries: enough to tell a born-digital page from
/// a scan, and a scan from one that already has an OCR layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct TextStats {
    /// Non-blank characters drawn visibly.
    pub visible_chars: usize,
    /// Non-blank characters drawn invisibly (text rendering modes 3 and 7),
    /// as OCR layers are.
    pub invisible_chars: usize,
    /// Glyphs with no Unicode meaning: text that is on the page but cannot be
    /// extracted (a font without a ToUnicode map, for instance).
    pub unmapped_glyphs: usize,
    /// Share of the page, 0–1, that images cover: close to 1 on a scan.
    /// Images drawn over one another count twice, up to the cap of 1.
    pub image_coverage: f64,
}

/// Count the text on one page (0-based).
pub fn page_text_stats(doc: &PdfDocument, page_index: usize) -> Result<TextStats> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = page_ids
        .get(page_index)
        .ok_or(PdfError::PageIndex(page_index))?;
    let content = page_content(doc, page_id)?;
    let resources = inherited_attribute(doc, page_id, "Resources")
        .and_then(|o| o.as_dict().cloned())
        .unwrap_or_default();
    let page = page_box(doc, page_id);
    let mut extractor = Extractor::new(doc);
    extractor.image_clip = Some(page);
    extractor.run(
        &content,
        &resources,
        Matrix::IDENTITY,
        0,
        TextState::default(),
    )?;
    let area = (page[2] - page[0]) * (page[3] - page[1]);
    let mut stats = extractor.stats;
    if area > 0.0 {
        stats.image_coverage = (extractor.image_area / area).min(1.0);
    }
    Ok(stats)
}

/// Extract text from every page.
pub fn extract_all_pages(doc: &PdfDocument) -> Result<Vec<String>> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    page_ids
        .iter()
        .map(|&id| extract_page_by_id(doc, id))
        .collect()
}

/// Extract text from one page (0-based).
pub fn extract_page_text(doc: &PdfDocument, page_index: usize) -> Result<String> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = page_ids
        .get(page_index)
        .ok_or(PdfError::PageIndex(page_index))?;
    extract_page_by_id(doc, page_id)
}

fn extract_page_by_id(doc: &PdfDocument, page_id: ObjectId) -> Result<String> {
    let content = page_content(doc, page_id)?;
    let resources = inherited_attribute(doc, page_id, "Resources")
        .and_then(|o| o.as_dict().cloned())
        .unwrap_or_default();
    let mut extractor = Extractor::new(doc);
    extractor.run(
        &content,
        &resources,
        Matrix::IDENTITY,
        0,
        TextState::default(),
    )?;
    Ok(extractor.finish())
}

pub(crate) fn extract_layout<'a>(
    doc: &'a PdfDocument,
    page_index: usize,
    loader: Option<&'a LayoutFontLoader<'a>>,
) -> Result<PageTextLayout> {
    let pages = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = pages
        .get(page_index)
        .ok_or(PdfError::PageIndex(page_index))?;
    let geometry = page_geometry(doc, page_id)?;
    let content = page_content(doc, page_id)?;
    let resources = inherited_attribute(doc, page_id, "Resources")
        .and_then(|o| o.as_dict().cloned())
        .unwrap_or_default();
    let mut extractor = Extractor::new(doc);
    extractor.geometry = Some(geometry);
    extractor.metric_loader = loader;
    extractor.run(
        &content,
        &resources,
        Matrix::IDENTITY,
        0,
        TextState::default(),
    )?;
    extractor.trim_output();
    let geometry = extractor.geometry.unwrap();
    Ok(PageTextLayout {
        text: extractor.out,
        width: geometry.width,
        height: geometry.height,
        glyphs: extractor.glyphs,
    })
}

/// Concatenated, decoded content streams of a page.
fn page_content(doc: &PdfDocument, page_id: ObjectId) -> Result<Vec<u8>> {
    let page = doc
        .resolve(page_id)
        .and_then(PdfObject::as_dict)
        .ok_or_else(|| PdfError::Structure("page object missing".into()))?;
    let mut out = Vec::new();
    match page.get("Contents").map(|c| doc.resolve_value(c)) {
        Some(PdfObject::Stream(stream)) => {
            out.extend_from_slice(&doc.stream_data(&stream)?);
        }
        Some(PdfObject::Array(items)) => {
            for item in items {
                if let PdfObject::Stream(stream) = doc.resolve_value(&item) {
                    out.extend_from_slice(&doc.stream_data(&stream)?);
                    out.push(b'\n');
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Inherited page attribute lookup (local to avoid a pdf_ops dependency).
pub(crate) fn inherited_attribute(
    doc: &PdfDocument,
    page_id: ObjectId,
    key: &str,
) -> Option<PdfObject> {
    let mut current = Some(page_id);
    for _ in 0..256 {
        let id = current?;
        let dict = doc.resolve(id).and_then(PdfObject::as_dict)?;
        if let Some(value) = dict.get(key) {
            let value = doc.resolve_value(value);
            // Null means an absent dictionary entry, including an indirect
            // null. Match the page tree used by the renderer.
            if !matches!(value, PdfObject::Null) {
                return Some(value);
            }
        }
        current = dict.get("Parent").and_then(PdfObject::as_ref);
    }
    None
}

struct Extractor<'a> {
    doc: &'a PdfDocument,
    font_cache: HashMap<String, LoadedFont>,
    next_scope: usize,
    geometry: Option<PageGeometry>,
    metric_loader: Option<&'a LayoutFontLoader<'a>>,
    glyphs: Vec<TextGlyph>,
    utf16_len: usize,
    last_baseline: Option<Baseline>,
    out: String,
    stats: TextStats,
    /// The page box, in user space, when image coverage is being measured.
    image_clip: Option<[f64; 4]>,
    /// Area of the page box that images cover, summed over every image.
    image_area: f64,
}

struct LoadedFont {
    text: Font,
    metrics: Option<Box<dyn LayoutFontMetrics>>,
    ascent: f64,
    descent: f64,
}

struct Baseline {
    end: (f64, f64),
    direction: (f64, f64),
    size: f64,
}

impl<'a> Extractor<'a> {
    fn new(doc: &'a PdfDocument) -> Self {
        Self {
            doc,
            font_cache: HashMap::new(),
            next_scope: 0,
            geometry: None,
            metric_loader: None,
            glyphs: Vec::new(),
            utf16_len: 0,
            last_baseline: None,
            out: String::new(),
            stats: TextStats::default(),
            image_clip: None,
            image_area: 0.0,
        }
    }

    fn finish(mut self) -> String {
        self.trim_output();
        self.out
    }

    fn trim_output(&mut self) {
        while self.out.ends_with(['\n', ' ']) {
            self.out.pop();
        }
        self.utf16_len = self.out.encode_utf16().count();
        self.glyphs.retain_mut(|glyph| {
            glyph.end = glyph.end.min(self.utf16_len);
            glyph.start < glyph.end
        });
    }

    fn append(&mut self, value: &str) {
        self.out.push_str(value);
        self.utf16_len += value.encode_utf16().count();
    }

    fn run(
        &mut self,
        content: &[u8],
        resources: &Dictionary,
        base_ctm: Matrix,
        depth: usize,
        mut state: TextState,
    ) -> Result<()> {
        if depth > 8 {
            return Ok(()); // form XObject recursion guard
        }
        let operations = match parse_content(content) {
            Ok(ops) => ops,
            Err(_) => return Ok(()), // tolerate broken content streams
        };

        let scope = self.next_scope;
        self.next_scope += 1;
        let mut ctm = base_ctm;
        let mut ctm_stack: Vec<Matrix> = Vec::new();
        let mut state_stack: Vec<TextState> = Vec::new();
        let mut text: Option<TextObject> = None;

        for Operation { operator, operands } in operations {
            match operator.as_str() {
                "q" => {
                    ctm_stack.push(ctm);
                    state_stack.push(state.clone());
                }
                "Q" => {
                    if let Some(m) = ctm_stack.pop() {
                        ctm = m;
                    }
                    if let Some(s) = state_stack.pop() {
                        state = s;
                    }
                }
                "cm" => {
                    if let Some(m) = matrix_from(&operands) {
                        ctm = m.multiply(&ctm);
                    }
                }
                "BT" => text = Some(TextObject::new()),
                "ET" => text = None,
                "Tc" => state.char_spacing = num(&operands, 0),
                "Tw" => state.word_spacing = num(&operands, 0),
                "Tz" => state.horiz_scale = num(&operands, 0) / 100.0,
                "TL" => state.leading = num(&operands, 0),
                "Ts" => state.rise = num(&operands, 0),
                "Tr" => {
                    state.render_mode = operands.first().and_then(PdfObject::as_i64).unwrap_or(0)
                }
                "Tf" => {
                    state.font_key = operands.first().and_then(|o| o.as_name()).map(|name| {
                        let key = format!("{scope}:{name}");
                        self.ensure_font(&key, name, resources);
                        key
                    });
                    state.font_size = num(&operands, 1);
                }
                "Td" => {
                    if let Some(t) = text.as_mut() {
                        t.translate_line(num(&operands, 0), num(&operands, 1));
                    }
                }
                "TD" => {
                    state.leading = -num(&operands, 1);
                    if let Some(t) = text.as_mut() {
                        t.translate_line(num(&operands, 0), num(&operands, 1));
                    }
                }
                "Tm" => {
                    if let (Some(m), Some(t)) = (matrix_from(&operands), text.as_mut()) {
                        t.set_matrix(m);
                    }
                }
                "T*" => {
                    if let Some(t) = text.as_mut() {
                        t.next_line(state.leading);
                    }
                }
                "Tj" => {
                    if let Some(bytes) = string_operand(&operands, 0) {
                        self.show_text(&bytes, &mut text, &state, &ctm);
                    }
                }
                "'" => {
                    if let Some(t) = text.as_mut() {
                        t.next_line(state.leading);
                    }
                    if let Some(bytes) = string_operand(&operands, 0) {
                        self.show_text(&bytes, &mut text, &state, &ctm);
                    }
                }
                "\"" => {
                    state.word_spacing = num(&operands, 0);
                    state.char_spacing = num(&operands, 1);
                    if let Some(t) = text.as_mut() {
                        t.next_line(state.leading);
                    }
                    if let Some(bytes) = string_operand(&operands, 2) {
                        self.show_text(&bytes, &mut text, &state, &ctm);
                    }
                }
                "TJ" => {
                    if let Some(PdfObject::Array(items)) = operands.first() {
                        for item in items {
                            match item {
                                PdfObject::LiteralString(b) | PdfObject::HexString(b) => {
                                    self.show_text(b, &mut text, &state, &ctm);
                                }
                                PdfObject::Integer(_) | PdfObject::Real(_) => {
                                    let adjust = match item {
                                        PdfObject::Integer(v) => *v as f64,
                                        PdfObject::Real(v) => *v,
                                        _ => 0.0,
                                    };
                                    let vertical = state
                                        .font_key
                                        .as_deref()
                                        .and_then(|k| self.font_cache.get(k))
                                        .is_some_and(|f| f.text.vertical);
                                    if let Some(t) = text.as_mut() {
                                        let shift = -adjust / 1000.0 * state.font_size;
                                        if vertical {
                                            t.advance_vertical(shift);
                                        } else {
                                            t.advance(shift * state.horiz_scale);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "Do" => {
                    if let Some(name) = operands.first().and_then(PdfObject::as_name) {
                        self.run_xobject(name, resources, ctm, depth, state.clone())?;
                    }
                }
                "BI" => self.note_image(ctm),
                _ => {}
            }
        }
        Ok(())
    }

    /// Follow a form XObject's content; note where an image XObject lands.
    fn run_xobject(
        &mut self,
        name: &str,
        resources: &Dictionary,
        ctm: Matrix,
        depth: usize,
        state: TextState,
    ) -> Result<()> {
        let Some(xobjects) = resources
            .get("XObject")
            .and_then(|x| self.doc.resolve_dict(x))
            .cloned()
        else {
            return Ok(());
        };
        let Some(PdfObject::Stream(stream)) = xobjects.get(name).map(|x| self.doc.resolve_value(x))
        else {
            return Ok(());
        };
        match stream
            .dictionary
            .get("Subtype")
            .and_then(PdfObject::as_name)
        {
            Some("Form") => {}
            Some("Image") => {
                self.note_image(ctm);
                return Ok(());
            }
            _ => return Ok(()),
        }
        let inner_ctm = stream
            .dictionary
            .get("Matrix")
            .map(|m| self.doc.resolve_value(m))
            .and_then(|m| match m {
                PdfObject::Array(items) => matrix_from(&items),
                _ => None,
            })
            .map(|m| m.multiply(&ctm))
            .unwrap_or(ctm);
        let inner_resources = stream
            .dictionary
            .get("Resources")
            .and_then(|r| self.doc.resolve_dict(r))
            .cloned()
            .unwrap_or_else(|| resources.clone());
        let data = self.doc.stream_data(&stream)?;
        self.run(&data, &inner_resources, inner_ctm, depth + 1, state)
    }

    /// Add the part of the page that an image drawn under `ctm` covers. An
    /// image fills the unit square of its own space.
    fn note_image(&mut self, ctm: Matrix) {
        let Some(page) = self.image_clip else {
            return;
        };
        let Some(image) = transformed_bounds([0.0, 0.0, 1.0, 1.0], ctm) else {
            return;
        };
        let width = image[2].min(page[2]) - image[0].max(page[0]);
        let height = image[3].min(page[3]) - image[1].max(page[1]);
        if width > 0.0 && height > 0.0 {
            self.image_area += width * height;
        }
    }

    fn ensure_font(&mut self, key: &str, name: &str, resources: &Dictionary) {
        if self.font_cache.contains_key(key) {
            return;
        }
        let dict = resources
            .get("Font")
            .and_then(|f| self.doc.resolve_dict(f))
            .and_then(|fonts| fonts.get(name))
            .and_then(|f| self.doc.resolve_dict(f));
        let text = dict
            .and_then(|d| load_font(self.doc, d).ok())
            .unwrap_or_default();
        let metrics = dict.and_then(|dict| self.metric_loader.map(|loader| loader(self.doc, dict)));
        let (ascent, descent) = dict
            .map(|d| font_vertical_metrics(self.doc, d))
            .unwrap_or((800.0, -200.0));
        self.font_cache.insert(
            key.to_owned(),
            LoadedFont {
                text,
                metrics,
                ascent,
                descent,
            },
        );
    }

    fn show_text(
        &mut self,
        bytes: &[u8],
        text: &mut Option<TextObject>,
        state: &TextState,
        ctm: &Matrix,
    ) {
        let Some(text) = text.as_mut() else {
            return; // show-text outside BT/ET: ignore
        };
        // Decode/measure first so the font borrow ends before output changes.
        let glyphs = {
            let default_font = Font::default();
            let loaded = state
                .font_key
                .as_deref()
                .and_then(|k| self.font_cache.get(k));
            let font = loaded.map(|f| &f.text).unwrap_or(&default_font);
            let mut glyphs = Vec::new();
            for code in font.codes(bytes) {
                let decoded = font.decode_code(code);
                let width = loaded
                    .and_then(|f| f.metrics.as_ref())
                    .map(|f| f.advance_width(code))
                    .unwrap_or_else(|| font.width(code));
                let spacing = state.char_spacing
                    + if font.is_space_code(code) {
                        state.word_spacing
                    } else {
                        0.0
                    };
                let mut bounds = [
                    0.0,
                    loaded.map(|f| f.descent).unwrap_or(-200.0),
                    width,
                    loaded.map(|f| f.ascent).unwrap_or(800.0),
                ];
                if let Some(ink) = loaded
                    .and_then(|f| f.metrics.as_ref())
                    .and_then(|f| f.glyph_bounds(code))
                {
                    bounds[0] = bounds[0].min(ink[0]);
                    bounds[1] = bounds[1].min(ink[1]);
                    bounds[2] = bounds[2].max(ink[2]);
                    bounds[3] = bounds[3].max(ink[3]);
                }
                let advance = if font.vertical {
                    // The text position is the glyph's vertical origin; its
                    // box hangs below it, offset by the position vector.
                    let (w1y, vx, vy) = font.vertical_metrics(code);
                    bounds = [
                        bounds[0] - vx,
                        bounds[1] - vy,
                        bounds[2] - vx,
                        bounds[3] - vy,
                    ];
                    w1y / 1000.0 * state.font_size + spacing
                } else {
                    (width / 1000.0 * state.font_size + spacing) * state.horiz_scale
                };
                glyphs.push((decoded, advance, bounds));
            }
            glyphs
        };
        let vertical = state
            .font_key
            .as_deref()
            .and_then(|k| self.font_cache.get(k))
            .is_some_and(|f| f.text.vertical);

        let has_text = glyphs.iter().any(|g| !g.0.is_empty());
        for (decoded, _, _) in &glyphs {
            if decoded.is_empty() {
                self.stats.unmapped_glyphs += 1;
                continue;
            }
            let chars = decoded.chars().filter(|c| !c.is_whitespace()).count();
            if state.is_invisible() {
                self.stats.invisible_chars += chars;
            } else {
                self.stats.visible_chars += chars;
            }
        }
        let user_matrix = text.text_matrix.multiply(ctm);
        if has_text {
            self.position_break(user_matrix, state);
        }
        for (decoded, advance, rect) in glyphs {
            let start = self.utf16_len;
            self.append(&decoded);
            if let Some(geometry) = &self.geometry {
                let transform = Matrix::new(
                    state.font_size * state.horiz_scale / 1000.0,
                    0.0,
                    0.0,
                    state.font_size / 1000.0,
                    0.0,
                    state.rise,
                )
                .multiply(&text.text_matrix)
                .multiply(ctm)
                .multiply(&geometry.transform);
                if let Some(mut bounds) = transformed_bounds(rect, transform) {
                    bounds[0] = bounds[0].clamp(0.0, geometry.width);
                    bounds[1] = bounds[1].clamp(0.0, geometry.height);
                    bounds[2] = bounds[2].clamp(0.0, geometry.width);
                    bounds[3] = bounds[3].clamp(0.0, geometry.height);
                    if start < self.utf16_len && bounds[2] > bounds[0] && bounds[3] > bounds[1] {
                        self.glyphs.push(TextGlyph {
                            start,
                            end: self.utf16_len,
                            bounds,
                        });
                    }
                }
            }
            if advance.is_finite() {
                if vertical {
                    text.advance_vertical(advance);
                } else {
                    text.advance(advance);
                }
            }
        }
        if has_text {
            // The "baseline" of vertical text runs down the column — text
            // space's −y — and its size is measured across it.
            let (along, across) = if vertical {
                (
                    (-user_matrix.c, -user_matrix.d),
                    (user_matrix.a, user_matrix.b),
                )
            } else {
                (
                    (user_matrix.a, user_matrix.b),
                    (user_matrix.c, user_matrix.d),
                )
            };
            let scale = along.0.hypot(along.1).max(1e-8);
            let sign = if vertical {
                state.font_size.signum()
            } else {
                (state.font_size * state.horiz_scale).signum()
            };
            self.last_baseline = Some(Baseline {
                end: text.position(ctm),
                direction: (along.0 / scale * sign, along.1 / scale * sign),
                size: (across.0.hypot(across.1) * state.font_size.abs()).max(1.0),
            });
        }
    }

    /// Insert spaces and newlines from the gap to the previous run, measured
    /// along and across the previous baseline instead of assuming all text
    /// runs horizontally. This keeps words split across TJ together on
    /// rotated text, such as an OCR layer on a page with /Rotate.
    fn position_break(&mut self, matrix: Matrix, state: &TextState) {
        let Some(last) = &self.last_baseline else {
            return;
        };
        let size = (matrix.c.hypot(matrix.d) * state.font_size.abs()).max(1.0);
        let dx = matrix.e - last.end.0;
        let dy = matrix.f - last.end.1;
        let across = (dx * -last.direction.1 + dy * last.direction.0).abs();
        let along = dx * last.direction.0 + dy * last.direction.1;
        if across > 0.5 * size.min(last.size) {
            if across > 1.8 * last.size {
                self.append("\n\n");
            } else {
                self.append("\n");
            }
        } else if along > 0.25 * size && !self.out.ends_with([' ', '\n']) && !self.out.is_empty() {
            self.append(" ");
        }
    }
}

fn num(operands: &[PdfObject], index: usize) -> f64 {
    match operands.get(index) {
        Some(PdfObject::Integer(v)) => *v as f64,
        Some(PdfObject::Real(v)) => *v,
        _ => 0.0,
    }
}

fn string_operand(operands: &[PdfObject], index: usize) -> Option<Vec<u8>> {
    match operands.get(index) {
        Some(PdfObject::LiteralString(b)) | Some(PdfObject::HexString(b)) => Some(b.clone()),
        _ => None,
    }
}

fn matrix_from(operands: &[PdfObject]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    let mut v = [0f64; 6];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = match &operands[i] {
            PdfObject::Integer(n) => *n as f64,
            PdfObject::Real(n) => *n,
            _ => return None,
        };
    }
    Some(Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]))
}

#[cfg(test)]
pub(crate) mod test_support {
    use pdf_core::document::PdfDocument;
    use pdf_core::object::{Dictionary, ObjectId, PdfObject};
    use pdf_core::stream::PdfStream;

    /// Build a one-page document whose content stream is `content`.
    pub fn doc_with_content(content: &[u8]) -> PdfDocument {
        let mut doc = PdfDocument::new_empty("1.7");

        // Not one of the standard 14: the layout tests reason in the flat
        // 500-unit default width, which Helvetica's real metrics would change.
        let mut font = Dictionary::new();
        font.insert("Type".into(), PdfObject::Name("Font".into()));
        font.insert("Subtype".into(), PdfObject::Name("Type1".into()));
        font.insert("BaseFont".into(), PdfObject::Name("TestSans".into()));
        font.insert("Encoding".into(), PdfObject::Name("WinAnsiEncoding".into()));
        let font_id = doc.add_object(PdfObject::Dictionary(font));

        let mut fonts = Dictionary::new();
        fonts.insert("F1".into(), PdfObject::Reference(font_id));
        let mut resources = Dictionary::new();
        resources.insert("Font".into(), PdfObject::Dictionary(fonts));

        let mut stream_dict = Dictionary::new();
        stream_dict.insert("Length".into(), PdfObject::Integer(content.len() as i64));
        let content_id = doc.add_object(PdfObject::Stream(PdfStream::new(
            stream_dict,
            content.to_vec(),
        )));

        let pages_id = ObjectId::new(50, 0);
        let mut page = Dictionary::new();
        page.insert("Type".into(), PdfObject::Name("Page".into()));
        page.insert("Parent".into(), PdfObject::Reference(pages_id));
        page.insert("Resources".into(), PdfObject::Dictionary(resources));
        page.insert("Contents".into(), PdfObject::Reference(content_id));
        let page_id = doc.add_object(PdfObject::Dictionary(page));

        let mut pages = Dictionary::new();
        pages.insert("Type".into(), PdfObject::Name("Pages".into()));
        pages.insert(
            "Kids".into(),
            PdfObject::Array(vec![PdfObject::Reference(page_id)]),
        );
        pages.insert("Count".into(), PdfObject::Integer(1));
        doc.set_object(pages_id, PdfObject::Dictionary(pages));

        let mut catalog = Dictionary::new();
        catalog.insert("Type".into(), PdfObject::Name("Catalog".into()));
        catalog.insert("Pages".into(), PdfObject::Reference(pages_id));
        let catalog_id = doc.add_object(PdfObject::Dictionary(catalog));
        doc.set_trailer_key("Root", PdfObject::Reference(catalog_id));
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::doc_with_content;
    use super::*;
    use pdf_core::stream::PdfStream;

    #[test]
    fn extracts_simple_text_with_spacing() {
        let doc =
            doc_with_content(b"BT /F1 12 Tf 72 720 Td (Hello) Tj 1 0 0 1 110 720 Tm (world) Tj ET");
        // Visible x gap between "Hello" end and "world" start -> space.
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Hello world");
    }

    #[test]
    fn newline_on_vertical_movement() {
        let doc =
            doc_with_content(b"BT /F1 12 Tf 72 720 Td (Line one) Tj 0 -14 Td (Line two) Tj ET");
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Line one\nLine two");
    }

    #[test]
    fn tj_array_and_quote_operators() {
        let doc = doc_with_content(b"BT /F1 12 Tf 14 TL 72 720 Td [(Wo) -30 (rld)] TJ (next) ' ET");
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "World\nnext");
    }

    #[test]
    fn flate_compressed_content_is_decoded() {
        use pdf_core::filter::flate_encode;
        let raw = b"BT /F1 12 Tf 72 720 Td (Compressed!) Tj ET";
        let compressed = flate_encode(raw);
        let mut doc = doc_with_content(b"");
        // Swap the content stream for a compressed one.
        let content_id = ObjectId::new(2, 0);
        let mut dict = Dictionary::new();
        dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
        dict.insert("Length".into(), PdfObject::Integer(compressed.len() as i64));
        doc.set_object(
            content_id,
            PdfObject::Stream(pdf_core::stream::PdfStream::new(dict, compressed)),
        );
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Compressed!");
    }

    #[test]
    fn rotated_word_runs_stay_on_one_line() {
        // An OCR layer on a /Rotate 90 page: each word positioned on its own,
        // advancing up the page.
        let doc = doc_with_content(
            b"BT /F1 12 Tf 0 1 -1 0 100 50 Tm (Rotated) Tj 0 1 -1 0 100 102 Tm (scan) Tj \
              0 1 -1 0 116 50 Tm (page) Tj ET",
        );
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Rotated scan\npage");
    }

    #[test]
    fn gaps_are_judged_against_the_scaled_font_size() {
        // A word split into two runs by a half-point kerning gap, set with a
        // unit font size scaled up by the text matrix.
        let doc = doc_with_content(
            b"BT /F1 1 Tf 12 0 0 12 72 720 Tm (Hel) Tj 12 0 0 12 90.5 720 Tm (lo) Tj \
              12 0 0 12 130 720 Tm (world) Tj ET",
        );
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Hello world");
    }

    #[test]
    fn text_stats_separate_visible_invisible_and_unmapped_text() {
        let doc = doc_with_content(b"BT /F1 12 Tf 72 720 Td (Seen it) Tj 3 Tr (OCR) Tj 0 Tr ET");
        let stats = page_text_stats(&doc, 0).unwrap();
        assert_eq!(
            stats,
            TextStats {
                visible_chars: 6,
                invisible_chars: 3,
                unmapped_glyphs: 0,
                image_coverage: 0.0,
            }
        );
        // Two-byte codes with no ToUnicode map decode to nothing.
        let mut doc = doc_with_content(b"BT /F1 12 Tf 72 720 Td <00010002> Tj ET");
        let font_id = ObjectId::new(1, 0);
        let mut font = doc.resolve(font_id).unwrap().as_dict().unwrap().clone();
        font.insert("Subtype".into(), PdfObject::Name("Type0".into()));
        font.insert("Encoding".into(), PdfObject::Name("Identity-H".into()));
        doc.set_object(font_id, PdfObject::Dictionary(font));
        assert_eq!(page_text_stats(&doc, 0).unwrap().unmapped_glyphs, 2);
    }

    #[test]
    fn text_stats_measure_how_much_of_the_page_images_cover() {
        let coverage = |content: &[u8]| {
            page_text_stats(&doc_with_content(content), 0)
                .unwrap()
                .image_coverage
        };
        assert_eq!(coverage(b"BT /F1 12 Tf 72 720 Td (Title) Tj ET"), 0.0);
        // Inline images: the left half of the page, then the whole of it
        // with a margin hanging off every edge.
        let half = coverage(b"q 306 0 0 792 0 0 cm BI /W 1 /H 1 /CS /G /BPC 8 ID \x00 EI Q");
        assert!((half - 0.5).abs() < 1e-9, "{half}");
        let all = coverage(b"q 700 0 0 900 -40 -50 cm BI /W 1 /H 1 /CS /G /BPC 8 ID \x00 EI Q");
        assert_eq!(all, 1.0);
    }

    #[test]
    fn image_xobjects_count_toward_coverage_and_forms_are_followed() {
        let mut doc = doc_with_content(b"q 612 0 0 396 0 0 cm /Im0 Do Q /Fm0 Do");
        let image = doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::from([
                ("Subtype".into(), PdfObject::Name("Image".into())),
                ("Width".into(), PdfObject::Integer(1)),
                ("Height".into(), PdfObject::Integer(1)),
                ("ColorSpace".into(), PdfObject::Name("DeviceGray".into())),
                ("BitsPerComponent".into(), PdfObject::Integer(8)),
            ]),
            vec![0],
        )));
        // The same image again, drawn by a form over the top quarter.
        let form = doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::from([
                ("Subtype".into(), PdfObject::Name("Form".into())),
                (
                    "BBox".into(),
                    PdfObject::Array(vec![
                        PdfObject::Integer(0),
                        PdfObject::Integer(0),
                        PdfObject::Integer(612),
                        PdfObject::Integer(792),
                    ]),
                ),
            ]),
            b"q 612 0 0 198 0 594 cm /Im0 Do Q".to_vec(),
        )));
        let page_id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
        let mut resources = page.get("Resources").unwrap().as_dict().unwrap().clone();
        resources.insert(
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([
                ("Im0".into(), PdfObject::Reference(image)),
                ("Fm0".into(), PdfObject::Reference(form)),
            ])),
        );
        page.insert("Resources".into(), PdfObject::Dictionary(resources));
        doc.set_object(page_id, PdfObject::Dictionary(page));
        let coverage = page_text_stats(&doc, 0).unwrap().image_coverage;
        assert!((coverage - 0.75).abs() < 1e-9, "{coverage}");
    }

    #[test]
    fn multipage_extraction() {
        let doc = doc_with_content(b"BT /F1 12 Tf 72 720 Td (Only page) Tj ET");
        let all = extract_all_pages(&doc).unwrap();
        assert_eq!(all, vec!["Only page".to_string()]);
        assert!(matches!(
            extract_page_text(&doc, 7),
            Err(PdfError::PageIndex(7))
        ));
    }
}
