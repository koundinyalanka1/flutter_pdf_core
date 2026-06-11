//! Milestone 8 (part 4): text extraction.
//!
//! Walks each page's content stream(s) with a faithful text-positioning
//! model and heuristics for word spacing and line breaks. Form XObjects are
//! followed (with a depth limit); inline images are skipped.

use std::collections::HashMap;

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};

use crate::content_stream::{parse_content, Operation};
use crate::font::{load_font, Font};
use crate::text_state::{Matrix, TextObject, TextState};

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
    extractor.run(&content, &resources, Matrix::IDENTITY, 0)?;
    Ok(extractor.finish())
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
fn inherited_attribute(doc: &PdfDocument, page_id: ObjectId, key: &str) -> Option<PdfObject> {
    let mut current = Some(page_id);
    for _ in 0..256 {
        let id = current?;
        let dict = doc.resolve(id).and_then(PdfObject::as_dict)?;
        if let Some(value) = dict.get(key) {
            return Some(doc.resolve_value(value));
        }
        current = dict.get("Parent").and_then(PdfObject::as_ref);
    }
    None
}

struct Extractor<'a> {
    doc: &'a PdfDocument,
    font_cache: HashMap<String, Font>,
    out: String,
    last_y: Option<f64>,
    last_x_end: f64,
    last_size: f64,
}

impl<'a> Extractor<'a> {
    fn new(doc: &'a PdfDocument) -> Self {
        Self {
            doc,
            font_cache: HashMap::new(),
            out: String::new(),
            last_y: None,
            last_x_end: 0.0,
            last_size: 12.0,
        }
    }

    fn finish(mut self) -> String {
        while self.out.ends_with(['\n', ' ']) {
            self.out.pop();
        }
        self.out
    }

    fn run(
        &mut self,
        content: &[u8],
        resources: &Dictionary,
        base_ctm: Matrix,
        depth: usize,
    ) -> Result<()> {
        if depth > 8 {
            return Ok(()); // form XObject recursion guard
        }
        let operations = match parse_content(content) {
            Ok(ops) => ops,
            Err(_) => return Ok(()), // tolerate broken content streams
        };

        let mut ctm = base_ctm;
        let mut ctm_stack: Vec<Matrix> = Vec::new();
        let mut state = TextState::default();
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
                "Tf" => {
                    state.font_key = operands.first().and_then(|o| o.as_name().map(str::to_owned));
                    state.font_size = num(&operands, 1);
                    if let Some(key) = state.font_key.clone() {
                        self.ensure_font(&key, resources);
                    }
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
                                    if let Some(t) = text.as_mut() {
                                        let tx = -adjust / 1000.0
                                            * state.font_size
                                            * state.horiz_scale;
                                        t.advance(tx);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "Do" => {
                    if let Some(name) = operands.first().and_then(PdfObject::as_name) {
                        self.run_form_xobject(name, resources, ctm, depth)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn run_form_xobject(
        &mut self,
        name: &str,
        resources: &Dictionary,
        ctm: Matrix,
        depth: usize,
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
        if stream.dictionary.get("Subtype").and_then(PdfObject::as_name) != Some("Form") {
            return Ok(());
        }
        let inner_ctm = stream
            .dictionary
            .get("Matrix")
            .and_then(|m| match m {
                PdfObject::Array(items) => matrix_from(items),
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
        self.run(&data, &inner_resources, inner_ctm, depth + 1)
    }

    fn ensure_font(&mut self, key: &str, resources: &Dictionary) {
        if self.font_cache.contains_key(key) {
            return;
        }
        let font = resources
            .get("Font")
            .and_then(|f| self.doc.resolve_dict(f))
            .and_then(|fonts| fonts.get(key))
            .and_then(|f| self.doc.resolve_dict(f))
            .and_then(|dict| load_font(self.doc, dict).ok())
            .unwrap_or_default();
        self.font_cache.insert(key.to_owned(), font);
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
        // Decode first so the font borrow ends before we mutate `self`.
        let (shown, advance_total) = {
            let default_font = Font::default();
            let font = state
                .font_key
                .as_deref()
                .and_then(|k| self.font_cache.get(k))
                .unwrap_or(&default_font);
            let mut shown = String::new();
            let mut advance_total = 0.0;
            for code in font.codes(bytes) {
                shown.push_str(&font.decode_code(code));
                let mut advance =
                    font.width(code) / 1000.0 * state.font_size + state.char_spacing;
                if font.is_space_code(code) {
                    advance += state.word_spacing;
                }
                advance_total += advance * state.horiz_scale;
            }
            (shown, advance_total)
        };

        let (x, y) = text.position(ctm);
        self.position_break(x, y, state.font_size.max(1.0));
        self.out.push_str(&shown);
        text.advance(advance_total);
        let (x_end, _) = text.position(ctm);
        self.last_x_end = x_end;
        self.last_size = state.font_size.max(1.0);
    }

    /// Insert spaces / newlines based on position deltas.
    fn position_break(&mut self, x: f64, y: f64, size: f64) {
        match self.last_y {
            None => {}
            Some(last_y) => {
                let dy = (last_y - y).abs();
                if dy > 0.5 * size.min(self.last_size) {
                    // Larger vertical gaps become paragraph breaks.
                    if dy > 1.8 * self.last_size {
                        self.out.push_str("\n\n");
                    } else {
                        self.out.push('\n');
                    }
                } else {
                    let gap = x - self.last_x_end;
                    if gap > 0.25 * size && !self.out.ends_with([' ', '\n']) && !self.out.is_empty()
                    {
                        self.out.push(' ');
                    }
                }
            }
        }
        self.last_y = Some(y);
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

        let mut font = Dictionary::new();
        font.insert("Type".into(), PdfObject::Name("Font".into()));
        font.insert("Subtype".into(), PdfObject::Name("Type1".into()));
        font.insert("BaseFont".into(), PdfObject::Name("Helvetica".into()));
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

    #[test]
    fn extracts_simple_text_with_spacing() {
        let doc = doc_with_content(
            b"BT /F1 12 Tf 72 720 Td (Hello) Tj 1 0 0 1 110 720 Tm (world) Tj ET",
        );
        // Visible x gap between "Hello" end and "world" start -> space.
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Hello world");
    }

    #[test]
    fn newline_on_vertical_movement() {
        let doc = doc_with_content(
            b"BT /F1 12 Tf 72 720 Td (Line one) Tj 0 -14 Td (Line two) Tj ET",
        );
        assert_eq!(extract_page_text(&doc, 0).unwrap(), "Line one\nLine two");
    }

    #[test]
    fn tj_array_and_quote_operators() {
        let doc = doc_with_content(
            b"BT /F1 12 Tf 14 TL 72 720 Td [(Wo) -30 (rld)] TJ (next) ' ET",
        );
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
