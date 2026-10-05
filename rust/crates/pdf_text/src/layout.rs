//! Selectable text in the displayed page's point coordinate system.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use serde::Serialize;

use crate::extractor::{extract_layout, inherited_attribute};
use crate::text_state::Matrix;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextGlyph {
    /// UTF-16 code-unit offsets into `PageTextLayout::text`, end exclusive.
    /// A ligature or surrogate pair may occupy more than one code unit.
    pub start: usize,
    pub end: usize,
    /// Axis-aligned [left, top, right, bottom], in displayed page points.
    pub bounds: [f64; 4],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageTextLayout {
    pub text: String,
    pub width: f64,
    pub height: f64,
    pub glyphs: Vec<TextGlyph>,
}

/// Lets a renderer supply the exact advances of a substituted font without
/// making the extraction crate depend on the rendering crate.
pub trait LayoutFontMetrics {
    /// Advance in 1000ths of an em, before character/word spacing.
    fn advance_width(&self, code: u32) -> f64;

    /// Optional actual glyph bounds in 1000ths of an em, with y pointing up.
    fn glyph_bounds(&self, _code: u32) -> Option<[f64; 4]> {
        None
    }
}

pub type LayoutFontLoader<'a> =
    dyn Fn(&PdfDocument, &Dictionary) -> Box<dyn LayoutFontMetrics> + 'a;

/// Page index is 0-based. Empty/scanned pages retain their displayed size and
/// return empty text/glyphs. Invisible OCR text is deliberately selectable.
pub fn extract_page_layout(doc: &PdfDocument, page_index: usize) -> Result<PageTextLayout> {
    extract_layout(doc, page_index, None)
}

/// As above, using the renderer's metrics for advances and glyph bounds.
pub fn extract_page_layout_with_metrics<'a>(
    doc: &'a PdfDocument,
    page_index: usize,
    loader: &'a LayoutFontLoader<'a>,
) -> Result<PageTextLayout> {
    extract_layout(doc, page_index, Some(loader))
}

pub(crate) struct PageGeometry {
    pub width: f64,
    pub height: f64,
    pub transform: Matrix,
}

/// Matches pdf_render::page's CropBox/MediaBox and flip-then-rotate transform.
pub(crate) fn page_geometry(doc: &PdfDocument, page_id: ObjectId) -> Result<PageGeometry> {
    let rect = page_rect(doc, page_id, "CropBox")
        .or_else(|| page_rect(doc, page_id, "MediaBox"))
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let width = (rect[2] - rect[0]).abs();
    let height = (rect[3] - rect[1]).abs();
    if !width.is_finite() || !height.is_finite() {
        return Err(PdfError::Structure("page dimensions must be finite".into()));
    }
    let (width, height) = (width.max(1.0), height.max(1.0));
    let rotate = inherited_attribute(doc, page_id, "Rotate")
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .rem_euclid(360);
    let flip = Matrix::new(1.0, 0.0, 0.0, -1.0, -rect[0], rect[3]);
    let rotation = match rotate {
        90 => Matrix::new(0.0, 1.0, -1.0, 0.0, height, 0.0),
        180 => Matrix::new(-1.0, 0.0, 0.0, -1.0, width, height),
        270 => Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, width),
        _ => Matrix::IDENTITY,
    };
    let (width, height) = if rotate == 90 || rotate == 270 {
        (height, width)
    } else {
        (width, height)
    };
    Ok(PageGeometry {
        width,
        height,
        transform: flip.multiply(&rotation),
    })
}

fn page_rect(doc: &PdfDocument, page_id: ObjectId, key: &str) -> Option<[f64; 4]> {
    let PdfObject::Array(values) = inherited_attribute(doc, page_id, key)? else {
        return None;
    };
    if values.len() < 4 {
        return None;
    }
    let mut rect = [0.0; 4];
    for (slot, value) in rect.iter_mut().zip(values.iter()) {
        *slot = number(&doc.resolve_value(value))?;
    }
    Some([
        rect[0].min(rect[2]),
        rect[1].min(rect[3]),
        rect[0].max(rect[2]),
        rect[1].max(rect[3]),
    ])
}

fn number(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(value) => Some(*value as f64),
        PdfObject::Real(value) => Some(*value),
        _ => None,
    }
}

pub(crate) fn font_vertical_metrics(doc: &PdfDocument, dict: &Dictionary) -> (f64, f64) {
    let descendant = dict.get("DescendantFonts").map(|o| doc.resolve_value(o));
    let owner = match &descendant {
        Some(PdfObject::Array(items)) => items.first().and_then(|o| doc.resolve_dict(o)),
        _ => None,
    }
    .unwrap_or(dict);
    let descriptor = owner
        .get("FontDescriptor")
        .and_then(|o| doc.resolve_dict(o));
    let metric = |name: &str| {
        descriptor
            .and_then(|d| d.get(name))
            .map(|o| doc.resolve_value(o))
            .and_then(|o| number(&o))
    };
    match (metric("Ascent"), metric("Descent")) {
        (Some(ascent), Some(descent))
            if ascent.is_finite() && descent.is_finite() && ascent > descent =>
        {
            (ascent, descent)
        }
        _ => (800.0, -200.0),
    }
}

pub(crate) fn transformed_bounds(rect: [f64; 4], matrix: Matrix) -> Option<[f64; 4]> {
    let points = [
        matrix.transform_point(rect[0], rect[1]),
        matrix.transform_point(rect[0], rect[3]),
        matrix.transform_point(rect[2], rect[1]),
        matrix.transform_point(rect[2], rect[3]),
    ];
    if points.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
        return None;
    }
    Some([
        points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
        points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
        points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
        points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::test_support::doc_with_content;
    use pdf_core::stream::PdfStream;

    fn numbers(values: &[i64]) -> PdfObject {
        PdfObject::Array(values.iter().copied().map(PdfObject::Integer).collect())
    }

    fn set_page(doc: &mut PdfDocument, key: &str, value: PdfObject) {
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(key.into(), value);
        doc.set_object(id, PdfObject::Dictionary(page));
    }

    fn assert_bounds(actual: [f64; 4], expected: [f64; 4]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-8, "{actual} != {expected}");
        }
    }

    #[test]
    fn crop_origin_and_all_page_rotations_use_displayed_coordinates() {
        for (rotation, size, bounds) in [
            (0, (100., 200.), [20., 152., 25., 162.]),
            (90, (200., 100.), [38., 20., 48., 25.]),
            (180, (100., 200.), [75., 38., 80., 48.]),
            (270, (200., 100.), [152., 75., 162., 80.]),
        ] {
            let mut doc = doc_with_content(b"BT /F1 10 Tf 30 60 Td (A) Tj ET");
            set_page(&mut doc, "CropBox", numbers(&[10, 20, 110, 220]));
            let rotate = doc.add_object(PdfObject::Integer(rotation));
            set_page(&mut doc, "Rotate", PdfObject::Reference(rotate));
            let layout = extract_page_layout(&doc, 0).unwrap();
            assert_eq!((layout.width, layout.height), size);
            assert_bounds(layout.glyphs[0].bounds, bounds);
        }
    }

    #[test]
    fn null_page_attributes_inherit_geometry_and_fonts() {
        let mut doc = doc_with_content(b"BT /F1 10 Tf 30 60 Td (A) Tj ET");
        let page_id = doc.collect_page_ids().unwrap()[0];
        let page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
        let parent_id = page.get("Parent").unwrap().as_ref().unwrap();
        let mut parent = doc.resolve(parent_id).unwrap().as_dict().unwrap().clone();
        parent.insert("Resources".into(), page["Resources"].clone());
        parent.insert("CropBox".into(), numbers(&[10, 20, 110, 220]));
        parent.insert("Rotate".into(), PdfObject::Integer(90));
        doc.set_object(parent_id, PdfObject::Dictionary(parent));
        let null = doc.add_object(PdfObject::Null);
        set_page(&mut doc, "CropBox", PdfObject::Reference(null));
        set_page(&mut doc, "Rotate", PdfObject::Null);
        set_page(&mut doc, "Resources", PdfObject::Null);
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!((layout.width, layout.height), (200., 100.));
        assert_eq!(layout.text, "A");
        assert_bounds(layout.glyphs[0].bounds, [38., 20., 48., 25.]);
    }

    #[test]
    fn text_matrix_ctm_rise_scaling_and_tj_spacing_combine() {
        let mut doc = doc_with_content(
            b"q 2 0 0 3 10 20 cm BT /F1 10 Tf 50 Tz 2 Tc 4 Ts 1 0 0 1 30 40 Tm [(A) -100 (B)] TJ ET Q",
        );
        set_page(&mut doc, "MediaBox", numbers(&[0, 0, 300, 400]));
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!(layout.text, "AB");
        assert_bounds(layout.glyphs[0].bounds, [70., 224., 75., 254.]);
        assert_bounds(layout.glyphs[1].bounds, [78., 224., 83., 254.]);
    }

    #[test]
    fn rotated_text_tj_runs_remain_one_word() {
        let doc = doc_with_content(b"BT /F1 10 Tf 0 1 -1 0 50 50 Tm [(Hel) -20 (lo)] TJ ET");
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!(layout.text, "Hello");
        assert_eq!(layout.glyphs.len(), 5);
    }

    #[test]
    fn unicode_clusters_use_utf16_offsets_and_hidden_ocr_is_selectable() {
        let mut doc = doc_with_content(b"BT /F1 10 Tf 3 Tr 30 60 Td <010203> Tj ET");
        let cmap = b"3 beginbfchar <01> <D83DDE00> <02> <00660069> <03> <0058> endbfchar";
        let cmap_id = doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::new(),
            cmap.to_vec(),
        )));
        let font_id = ObjectId::new(1, 0);
        let mut font = doc.resolve(font_id).unwrap().as_dict().unwrap().clone();
        font.insert("ToUnicode".into(), PdfObject::Reference(cmap_id));
        doc.set_object(font_id, PdfObject::Dictionary(font));
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!(layout.text, "😀fiX");
        assert_eq!(
            layout
                .glyphs
                .iter()
                .map(|g| (g.start, g.end))
                .collect::<Vec<_>>(),
            vec![(0, 2), (2, 4), (4, 5)]
        );
    }

    #[test]
    fn form_fonts_are_scoped_and_form_matrix_is_applied() {
        let mut doc = doc_with_content(
            b"BT /F1 10 Tf 20 20 Td (A) Tj ET /Form Do BT /F1 10 Tf 20 40 Td (A) Tj ET",
        );
        let mut font = doc
            .resolve(ObjectId::new(1, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        font.insert("FirstChar".into(), PdfObject::Integer(65));
        font.insert("Widths".into(), numbers(&[900]));
        let font_id = doc.add_object(PdfObject::Dictionary(font));
        let resources = Dictionary::from([(
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "F1".into(),
                PdfObject::Reference(font_id),
            )])),
        )]);
        let form = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Form".into())),
            ("Resources".into(), PdfObject::Dictionary(resources)),
            ("Matrix".into(), numbers(&[2, 0, 0, 2, 50, 50])),
        ]);
        let form_id = doc.add_object(PdfObject::Stream(PdfStream::new(
            form,
            b"BT /F1 10 Tf 20 20 Td (A) Tj ET".to_vec(),
        )));
        let page_id = doc.collect_page_ids().unwrap()[0];
        let mut resources = doc.resolve(page_id).unwrap().as_dict().unwrap()["Resources"]
            .as_dict()
            .unwrap()
            .clone();
        resources.insert(
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "Form".into(),
                PdfObject::Reference(form_id),
            )])),
        );
        set_page(&mut doc, "Resources", PdfObject::Dictionary(resources));
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!(layout.glyphs.len(), 3);
        assert_bounds(layout.glyphs[0].bounds, [20., 764., 25., 774.]);
        assert_bounds(layout.glyphs[1].bounds, [90., 686., 108., 706.]);
        assert_bounds(layout.glyphs[2].bounds, [20., 744., 25., 754.]);
    }

    #[test]
    fn blank_page_has_size_without_selectable_text_and_invalid_page_errors() {
        let doc = doc_with_content(b"0 0 100 100 re f");
        let layout = extract_page_layout(&doc, 0).unwrap();
        assert_eq!((layout.width, layout.height), (612., 792.));
        assert!(layout.text.is_empty() && layout.glyphs.is_empty());
        assert!(matches!(
            extract_page_layout(&doc, 1),
            Err(PdfError::PageIndex(1))
        ));
    }
}
