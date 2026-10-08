//! OCR for PDF pages: decide which pages need it, render and recognize them,
//! and write what was read back into the document as invisible text.
//!
//! Coordinates here are displayed page points with a top-left origin: the
//! space `pdf_page_text_layout_json` reports glyphs in, after CropBox and
//! /Rotate. Results can also come from another engine (`apply_ocr`), so they
//! are validated like any other untrusted input.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_ops::page_tree::effective_page_dict;
use pdf_render::{page_size_points, render_page, RenderOptions, RenderSize};
use pdf_text::extractor::page_text_stats;
use pdf_text::layout::{extract_page_layout, PageTextLayout};
use serde::{Deserialize, Serialize};

use crate::engine::{OcrEngine, OcrOptions};
use crate::image::GrayImage;
use crate::layer::{add_text_layers, PageWords, WordBox};
use crate::result::OcrLine;

/// Rendering resolution for recognition: enough for 6-point text.
pub const DEFAULT_DPI: f64 = 300.0;
/// Pixels per rendered page at most. An A4 page at 300 dpi is 8.7 million;
/// larger pages are recognized at a lower resolution rather than at the cost
/// of a buffer a phone cannot spare.
const MAX_PIXELS: usize = 12_000_000;
/// Limits on results handed in from elsewhere.
const MAX_WORDS_PER_PAGE: usize = 50_000;
const MAX_WORD_CHARS: usize = 1_000;

#[derive(Clone, Debug)]
pub struct PdfOcrOptions {
    pub dpi: f64,
    /// Recognize pages that already have text, too.
    pub force: bool,
    pub engine: OcrOptions,
}

impl Default for PdfOcrOptions {
    fn default() -> Self {
        Self {
            dpi: DEFAULT_DPI,
            force: false,
            engine: OcrOptions::default(),
        }
    }
}

/// Why a page was left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PageStatus {
    /// Recognized; its text layer was written.
    Recognized,
    /// Born-digital text is already there.
    HasText,
    /// A previous OCR layer (invisible text) is already there.
    HasOcrLayer,
    /// Recognized, but nothing legible was found.
    NoText,
}

/// Whether a page already carries text, so recognizing it would duplicate it.
pub fn existing_text(doc: &PdfDocument, page_index: usize) -> Result<Option<PageStatus>> {
    let stats = page_text_stats(doc, page_index)?;
    Ok(if stats.invisible_chars >= 20 {
        Some(PageStatus::HasOcrLayer)
    } else if stats.visible_chars >= 50 && stats.unmapped_glyphs <= stats.visible_chars {
        Some(PageStatus::HasText)
    } else {
        None
    })
}

/// One page's recognized text, in displayed page points.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageOcr {
    #[serde(skip)]
    pub page_index: usize,
    pub width: f64,
    pub height: f64,
    /// Resolution the page was recognized at.
    pub dpi: f64,
    pub skew_degrees: f64,
    pub orientation_degrees: u16,
    pub lines: Vec<OcrLine>,
}

impl PageOcr {
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn word_count(&self) -> usize {
        self.lines.iter().map(|l| l.words.len()).sum()
    }

    pub fn mean_confidence(&self) -> f32 {
        let words: Vec<f32> = self
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.confidence))
            .collect();
        if words.is_empty() {
            0.0
        } else {
            words.iter().sum::<f32>() / words.len() as f32
        }
    }

    pub fn words(&self) -> PageWords {
        PageWords {
            page_index: self.page_index,
            lines: self
                .lines
                .iter()
                .map(|l| {
                    l.words
                        .iter()
                        .map(|w| WordBox {
                            text: w.text.clone(),
                            quad: w.quad,
                        })
                        .collect()
                })
                .collect(),
        }
    }
}

/// Render page `page_index` (0-based) and recognize it.
pub fn recognize_page(
    doc: &PdfDocument,
    page_index: usize,
    engine: &OcrEngine,
    options: &PdfOcrOptions,
) -> Result<PageOcr> {
    if !(options.dpi.is_finite() && (36.0..=1200.0).contains(&options.dpi)) {
        return Err(PdfError::Structure(
            "OCR resolution must be between 36 and 1200 dpi".into(),
        ));
    }
    let (width, height) = page_size_points(doc, page_index)?;
    let render = render_page(
        doc,
        page_index,
        RenderOptions {
            size: RenderSize::Scale(options.dpi / 72.0),
            max_pixels: MAX_PIXELS,
            ..Default::default()
        },
    )?;
    let image = GrayImage::from_rgba(
        render.width as usize,
        render.height as usize,
        &render.pixels,
    )
    .ok_or_else(|| PdfError::Structure("rendered page has an unexpected size".into()))?;
    drop(render);
    let page = engine.recognize(&image, &options.engine);
    let (fx, fy) = (width / image.width as f64, height / image.height as f64);
    let page = page.scaled(fx, fy);
    Ok(PageOcr {
        page_index,
        width,
        height,
        dpi: 72.0 / fx,
        skew_degrees: page.skew_degrees,
        orientation_degrees: page.orientation_degrees,
        lines: page.lines,
    })
}

/// The selectable text a layer of these words gives, without changing the
/// document: the layer is written into a one-page copy of the page's
/// geometry and measured by the same code that measures saved files.
pub fn ocr_layout(doc: &PdfDocument, page: &PageOcr) -> Result<PageTextLayout> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = page_ids
        .get(page.page_index)
        .ok_or(PdfError::PageIndex(page.page_index))?;
    let source = effective_page_dict(doc, page_id)?;
    let mut probe = PdfDocument::new_empty("1.7");
    let mut dict = Dictionary::from([("Type".into(), PdfObject::Name("Page".into()))]);
    for key in ["MediaBox", "CropBox", "Rotate"] {
        if let Some(value) = source.get(key) {
            dict.insert(key.into(), resolved(doc, value));
        }
    }
    let empty = probe.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        Vec::new(),
    )));
    dict.insert("Contents".into(), PdfObject::Reference(empty));
    install_single_page(&mut probe, dict);
    let mut words = page.words();
    words.page_index = 0;
    add_text_layers(&mut probe, &[words])?;
    extract_page_layout(&probe, 0)
}

/// A value with references resolved, arrays element by element (a MediaBox
/// may hold indirect numbers).
fn resolved(doc: &PdfDocument, value: &PdfObject) -> PdfObject {
    match doc.resolve_value(value) {
        PdfObject::Array(items) => {
            PdfObject::Array(items.iter().map(|v| doc.resolve_value(v)).collect())
        }
        other => other,
    }
}

fn install_single_page(doc: &mut PdfDocument, mut page: Dictionary) {
    let pages_id = doc.add_object(PdfObject::Null);
    page.insert("Parent".into(), PdfObject::Reference(pages_id));
    let page_id = doc.add_object(PdfObject::Dictionary(page));
    doc.set_object(
        pages_id,
        PdfObject::Dictionary(Dictionary::from([
            ("Type".into(), PdfObject::Name("Pages".into())),
            (
                "Kids".into(),
                PdfObject::Array(vec![PdfObject::Reference(page_id)]),
            ),
            ("Count".into(), PdfObject::Integer(1)),
        ])),
    );
    let catalog = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Catalog".into())),
        ("Pages".into(), PdfObject::Reference(pages_id)),
    ])));
    doc.set_trailer_key("Root", PdfObject::Reference(catalog));
}

/// What happened to one page of a searchable-PDF job (pages are 1-based).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageReport {
    pub page: usize,
    pub status: PageStatus,
    pub words: usize,
    pub confidence: f32,
}

/// Recognize the given pages (0-based) and write text layers onto them.
/// Pages that already have text are skipped unless `options.force`.
pub fn make_searchable(
    doc: &mut PdfDocument,
    pages: &[usize],
    engine: &OcrEngine,
    options: &PdfOcrOptions,
) -> Result<Vec<PageReport>> {
    let mut reports = Vec::with_capacity(pages.len());
    let mut layers = Vec::new();
    for &page_index in pages {
        let report = |status, words, confidence| PageReport {
            page: page_index + 1,
            status,
            words,
            confidence,
        };
        if !options.force {
            if let Some(status) = existing_text(doc, page_index)? {
                reports.push(report(status, 0, 0.0));
                continue;
            }
        }
        let page = recognize_page(doc, page_index, engine, options)?;
        if page.lines.is_empty() {
            reports.push(report(PageStatus::NoText, 0, 0.0));
            continue;
        }
        reports.push(report(
            PageStatus::Recognized,
            page.word_count(),
            page.mean_confidence(),
        ));
        layers.push(page.words());
    }
    add_text_layers(doc, &layers)?;
    Ok(reports)
}

/// OCR results from any engine, as `pdf_ocr_page_json` reports them: pages
/// are 1-based; words carry a `quad` or, failing that, `bounds`, in
/// displayed page points.
#[derive(Clone, Debug, Deserialize)]
pub struct ExternalPage {
    pub page: usize,
    pub lines: Vec<ExternalLine>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExternalLine {
    pub words: Vec<ExternalWord>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExternalWord {
    pub text: String,
    #[serde(default)]
    pub quad: Option<[[f64; 2]; 4]>,
    #[serde(default)]
    pub bounds: Option<[f64; 4]>,
}

/// Write text layers from results computed elsewhere. Returns the number of
/// pages that received one.
pub fn apply_ocr(doc: &mut PdfDocument, pages: &[ExternalPage]) -> Result<usize> {
    let count = doc.page_count().unwrap_or(0) as usize;
    let mut layers = Vec::with_capacity(pages.len());
    for page in pages {
        if page.page == 0 || page.page > count {
            return Err(PdfError::PageIndex(page.page.saturating_sub(1)));
        }
        let words: usize = page.lines.iter().map(|l| l.words.len()).sum();
        if words > MAX_WORDS_PER_PAGE {
            return Err(PdfError::Structure(format!(
                "page {} has more than {MAX_WORDS_PER_PAGE} words",
                page.page
            )));
        }
        let mut lines = Vec::with_capacity(page.lines.len());
        for line in &page.lines {
            let mut boxes = Vec::with_capacity(line.words.len());
            for word in &line.words {
                if word.text.chars().count() > MAX_WORD_CHARS
                    || word.text.chars().any(char::is_control)
                {
                    return Err(PdfError::Structure(format!(
                        "page {}: unusable word text",
                        page.page
                    )));
                }
                let quad = match (word.quad, word.bounds) {
                    (Some(quad), _) => quad,
                    (None, Some([l, t, r, b])) => [[l, t], [r, t], [r, b], [l, b]],
                    (None, None) => {
                        return Err(PdfError::Structure(format!(
                            "page {}: a word has no position",
                            page.page
                        )))
                    }
                };
                if quad
                    .iter()
                    .flatten()
                    .any(|v| !v.is_finite() || v.abs() > 1e6)
                {
                    return Err(PdfError::Structure(format!(
                        "page {}: a word's position is not finite",
                        page.page
                    )));
                }
                boxes.push(WordBox {
                    text: word.text.clone(),
                    quad,
                });
            }
            lines.push(boxes);
        }
        layers.push(PageWords {
            page_index: page.page - 1,
            lines,
        });
    }
    add_text_layers(doc, &layers)
}
