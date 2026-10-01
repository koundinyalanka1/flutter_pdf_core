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
    Ok(PageGeometry { width, height, transform: flip.multiply(&rotation) })
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
    Some([rect[0].min(rect[2]), rect[1].min(rect[3]), rect[0].max(rect[2]), rect[1].max(rect[3])])
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
    }.unwrap_or(dict);
    let descriptor = owner.get("FontDescriptor").and_then(|o| doc.resolve_dict(o));
    let metric = |name: &str| descriptor.and_then(|d| d.get(name))
        .map(|o| doc.resolve_value(o)).and_then(|o| number(&o));
    match (metric("Ascent"), metric("Descent")) {
        (Some(ascent), Some(descent)) if ascent.is_finite() && descent.is_finite() && ascent > descent =>
            (ascent, descent),
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
