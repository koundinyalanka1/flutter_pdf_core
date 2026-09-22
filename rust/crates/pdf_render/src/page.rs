//! Content-stream execution: turns a page into pixels.
//!
//! Reuses `pdf_text::content_stream::parse_content` for the operator stream
//! and `pdf_text::font` for metrics, so the renderer and the text extractor
//! agree on how a page is structured.
//!
//! Supported: the graphics state stack, path construction and painting
//! (fill/stroke/even-odd), arbitrary clipping paths, DeviceGray/RGB/CMYK plus
//! ICCBased/Indexed/Separation colour, constant alpha from `/ExtGState`,
//! image XObjects (JPEG, CCITT G3/G4, Flate, LZW, stencil masks, soft masks),
//! form XObjects, and text in TrueType or CFF outlines.
//!
//! Text whose font the document did not embed — the standard 14, or a program
//! in a format this renderer cannot parse — is drawn in a substitute face; see
//! [`crate::font::fallback`]. It is drawn, not skipped, because a page of
//! invisible text is indistinguishable from a broken file.
//!
//! Not supported, and deliberately skipped rather than failed: shading
//! patterns (`sh`), tiling patterns, inline images (`BI…EI`), blend modes and
//! JPX/JBIG2 image codecs. Pages using those render with everything else
//! intact, and [`RenderedPage::warnings`] says what was left out.

use std::collections::HashMap;
use std::rc::Rc;

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_ops::page_tree::effective_page_dict;
use pdf_text::content_stream::{parse_content, Operation};

use crate::canvas::{stroke_outline, Canvas, ClipMask, Rgb};
use crate::font::RenderFont;
use crate::geom::{FillRule, Matrix, Path};
use crate::image::decode_image;

/// How large to render.
#[derive(Debug, Clone, Copy)]
pub enum RenderSize {
    /// Multiply the page's point size by this finite, positive factor (1.0 = 72 dpi).
    Scale(f64),
    /// Fit inside this nonzero pixel box, preserving aspect ratio where possible.
    FitBox { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub size: RenderSize,
    pub background: Rgb,
    /// Hard cap on output pixels, so a malformed /MediaBox cannot ask for a
    /// gigapixel buffer on a phone. Must be at least one.
    pub max_pixels: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            size: RenderSize::Scale(1.0),
            background: Rgb::new(1.0, 1.0, 1.0),
            max_pixels: 32_000_000,
        }
    }
}

pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top-left origin.
    pub pixels: Vec<u8>,
    /// Things that were skipped while drawing — an image codec this build
    /// cannot read, a font with no usable outlines. The page still rendered,
    /// but it is not a faithful copy, and a caller that shows it should be
    /// able to say so instead of presenting a silently incomplete page.
    pub warnings: Vec<String>,
}

/// Rasterize page `index` (0-based).
pub fn render_page(
    doc: &PdfDocument,
    index: usize,
    options: RenderOptions,
) -> Result<RenderedPage> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = page_ids.get(index).ok_or(PdfError::PageIndex(index))?;
    let page = effective_page_dict(doc, page_id)?;

    // CropBox wins over MediaBox for display; fall back to US Letter.
    let box_rect = rect(doc, &page, "CropBox")
        .or_else(|| rect(doc, &page, "MediaBox"))
        .unwrap_or((0.0, 0.0, 612.0, 792.0));
    let (page_w, page_h) = box_dimensions(box_rect)?;

    let rotate = page
        .get("Rotate")
        .map(|o| doc.resolve_value(o))
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .rem_euclid(360);
    let swapped = rotate == 90 || rotate == 270;
    let (display_w, display_h) = if swapped {
        (page_h, page_w)
    } else {
        (page_w, page_h)
    };

    let (out_w, out_h) = output_dimensions(display_w, display_h, options)?;

    let mut canvas = Canvas::new(out_w, out_h);
    canvas.fill_background(options.background);

    // PDF user space is y-up with the origin at the crop box's lower-left;
    // the canvas is y-down from the top-left. Translate, flip, then rotate.
    let flip = Matrix::new(1.0, 0.0, 0.0, -1.0, -box_rect.0, box_rect.3);
    let rotation = match rotate {
        90 => Matrix::new(0.0, 1.0, -1.0, 0.0, page_h, 0.0),
        180 => Matrix::new(-1.0, 0.0, 0.0, -1.0, page_w, page_h),
        270 => Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, page_w),
        _ => Matrix::IDENTITY,
    };
    let device_scale = Matrix::scale(out_w as f64 / display_w, out_h as f64 / display_h);
    let base_ctm = flip.then(&rotation).then(&device_scale);

    let resources = page
        .get("Resources")
        .map(|o| doc.resolve_value(o))
        .and_then(|o| doc.resolve_dict(&o).cloned())
        .unwrap_or_default();
    let content = page_content(doc, &page)?;

    // Every content stream failed to decode, so there is nothing to draw and
    // no honest way to call the result a render. Returning white pixels here
    // is what made unsupported filters look like empty documents; the caller
    // needs the reason so it can say what actually went wrong.
    if content.streams > 0 && content.failed == content.streams {
        let reason = content
            .first_error
            .unwrap_or_else(|| "content stream could not be decoded".to_owned());
        return Err(PdfError::Filter(format!(
            "page {} has no decodable content: {reason}",
            index + 1
        )));
    }

    let mut renderer = Renderer {
        doc,
        canvas: &mut canvas,
        fonts: HashMap::new(),
        depth: 0,
        warnings: Vec::new(),
    };
    let mut state = GraphicsState::new(base_ctm);
    // Parsing recovers from damage rather than failing, so this only errors
    // in cases the renderer genuinely cannot proceed from; the page
    // background is still a better answer than no page at all.
    let _ = renderer.run(&content.data, &resources, &mut state);

    let mut warnings = renderer.warnings;
    if content.failed > 0 {
        warnings.push(format!(
            "{} of {} content streams could not be decoded",
            content.failed, content.streams
        ));
    }
    warnings.sort();
    warnings.dedup();

    Ok(RenderedPage {
        width: out_w as u32,
        height: out_h as u32,
        pixels: canvas.pixels,
        warnings,
    })
}

/// Page size in PostScript points, accounting for `/Rotate`.
pub fn page_size_points(doc: &PdfDocument, index: usize) -> Result<(f64, f64)> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let &page_id = page_ids.get(index).ok_or(PdfError::PageIndex(index))?;
    let page = effective_page_dict(doc, page_id)?;
    let r = rect(doc, &page, "CropBox")
        .or_else(|| rect(doc, &page, "MediaBox"))
        .unwrap_or((0.0, 0.0, 612.0, 792.0));
    let (w, h) = box_dimensions(r)?;
    let rotate = page
        .get("Rotate")
        .map(|o| doc.resolve_value(o))
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .rem_euclid(360);
    Ok(if rotate == 90 || rotate == 270 {
        (h, w)
    } else {
        (w, h)
    })
}

fn box_dimensions(rect: (f64, f64, f64, f64)) -> Result<(f64, f64)> {
    let width = (rect.2 - rect.0).abs();
    let height = (rect.3 - rect.1).abs();
    if !width.is_finite() || !height.is_finite() {
        return Err(PdfError::Structure("page dimensions must be finite".into()));
    }
    Ok((width.max(1.0), height.max(1.0)))
}

fn output_dimensions(width: f64, height: f64, options: RenderOptions) -> Result<(usize, usize)> {
    let requested_scale = match options.size {
        RenderSize::Scale(scale) if scale.is_finite() && scale > 0.0 => scale,
        RenderSize::FitBox {
            width: w,
            height: h,
        } if w > 0 && h > 0 => (w as f64 / width).min(h as f64 / height),
        _ => {
            return Err(PdfError::Structure(
                "render size must be finite and positive".into(),
            ))
        }
    };
    // RGBA allocations must fit Rust's allocation limit as well as the caller's
    // pixel budget. Bound the scale before multiplying any dimensions.
    // Canvas also allocates a coverage row of width + 2 f32 elements.
    let budget = options.max_pixels.min(isize::MAX as usize / 4 - 2);
    if budget == 0 {
        return Err(PdfError::Structure(
            "render pixel budget must be positive".into(),
        ));
    }
    let side_limit = budget.min(u32::MAX as usize);
    let scale = requested_scale
        .min((budget as f64).sqrt() / width.sqrt() / height.sqrt())
        .min(side_limit as f64 / width)
        .min(side_limit as f64 / height);
    let mut out_w = ((width * scale).round().max(1.0) as usize).min(side_limit);
    let mut out_h = ((height * scale).round().max(1.0) as usize).min(side_limit);
    // Rounding and the one-pixel minimum can raise the area above the budget,
    // especially on very narrow pages. Division avoids overflowing w * h.
    if out_w > budget / out_h {
        if out_w >= out_h {
            out_w = budget / out_h;
        } else {
            out_h = budget / out_w;
        }
    }
    Ok((out_w, out_h))
}

// ---------------------------------------------------------------------------
// Graphics state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GraphicsState {
    ctm: Matrix,
    fill: Rgb,
    stroke: Rgb,
    fill_components: usize,
    stroke_components: usize,
    line_width: f64,
    fill_alpha: f32,
    stroke_alpha: f32,
    clip: ClipMask,
    font: Option<Rc<RenderFont>>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    horizontal_scale: f64,
    leading: f64,
    rise: f64,
    render_mode: i64,
    text_matrix: Matrix,
    line_matrix: Matrix,
}

impl GraphicsState {
    fn new(ctm: Matrix) -> Self {
        Self {
            ctm,
            fill: Rgb::BLACK,
            stroke: Rgb::BLACK,
            fill_components: 1,
            stroke_components: 1,
            line_width: 1.0,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            clip: None,
            font: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
            text_matrix: Matrix::IDENTITY,
            line_matrix: Matrix::IDENTITY,
        }
    }
}

struct Renderer<'a> {
    doc: &'a PdfDocument,
    canvas: &'a mut Canvas,
    fonts: HashMap<String, Rc<RenderFont>>,
    depth: usize,
    warnings: Vec<String>,
}

impl Renderer<'_> {
    /// Record something the page needed but this build could not draw.
    ///
    /// Deduplicated on the way in, not just on the way out: `show_text` runs
    /// once per text-showing operator, so a page of substituted text would
    /// otherwise spend the whole cap on copies of one message and crowd out
    /// the distinct ones — a skipped image, say — that come later.
    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.warnings.len() >= 32 || self.warnings.iter().any(|w| *w == message) {
            return;
        }
        self.warnings.push(message);
    }

    fn run(
        &mut self,
        content: &[u8],
        resources: &Dictionary,
        state: &mut GraphicsState,
    ) -> Result<()> {
        let operations = parse_content(content)?;
        let mut stack: Vec<GraphicsState> = Vec::new();
        let mut path = Path::with_tolerance(0.25);
        let mut pending_clip: Option<FillRule> = None;

        for op in &operations {
            let n = |i: usize| number(op, i);
            match op.operator.as_str() {
                // -- graphics state ------------------------------------------
                "q" => stack.push(state.clone()),
                "Q" => {
                    if let Some(previous) = stack.pop() {
                        *state = previous;
                    }
                }
                "cm" => {
                    if op.operands.len() >= 6 {
                        let m = Matrix::new(n(0), n(1), n(2), n(3), n(4), n(5));
                        state.ctm = m.then(&state.ctm);
                    }
                }
                "w" => state.line_width = n(0),
                "gs" => self.apply_ext_gstate(op, resources, state),

                // -- path construction ---------------------------------------
                "m" => path.move_to(n(0), n(1)),
                "l" => path.line_to(n(0), n(1)),
                "c" => path.curve_to(n(0), n(1), n(2), n(3), n(4), n(5)),
                "v" => {
                    let current = path.current_point().unwrap_or(crate::geom::Point::new(n(0), n(1)));
                    path.curve_to(current.x, current.y, n(0), n(1), n(2), n(3));
                }
                "y" => path.curve_to(n(0), n(1), n(2), n(3), n(2), n(3)),
                "h" => path.close(),
                "re" => path.rect(n(0), n(1), n(2), n(3)),

                // -- clipping -------------------------------------------------
                "W" => pending_clip = Some(FillRule::NonZero),
                "W*" => pending_clip = Some(FillRule::EvenOdd),

                // -- path painting --------------------------------------------
                "n" | "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" => {
                    self.paint(&op.operator, &mut path, state, &mut pending_clip);
                    path = Path::with_tolerance(0.25);
                }

                // -- colour ----------------------------------------------------
                "g" => {
                    state.fill = Rgb::gray(n(0) as f32);
                    state.fill_components = 1;
                }
                "G" => {
                    state.stroke = Rgb::gray(n(0) as f32);
                    state.stroke_components = 1;
                }
                "rg" => {
                    state.fill = Rgb::new(n(0) as f32, n(1) as f32, n(2) as f32);
                    state.fill_components = 3;
                }
                "RG" => {
                    state.stroke = Rgb::new(n(0) as f32, n(1) as f32, n(2) as f32);
                    state.stroke_components = 3;
                }
                "k" => {
                    state.fill =
                        Rgb::from_cmyk(n(0) as f32, n(1) as f32, n(2) as f32, n(3) as f32);
                    state.fill_components = 4;
                }
                "K" => {
                    state.stroke =
                        Rgb::from_cmyk(n(0) as f32, n(1) as f32, n(2) as f32, n(3) as f32);
                    state.stroke_components = 4;
                }
                "cs" | "CS" => {
                    let components = self.space_components(op, resources);
                    let black = Rgb::BLACK;
                    if op.operator == "cs" {
                        state.fill_components = components;
                        state.fill = black;
                    } else {
                        state.stroke_components = components;
                        state.stroke = black;
                    }
                }
                "sc" | "scn" | "SC" | "SCN" => {
                    let values: Vec<f64> = op.operands.iter().filter_map(as_number).collect();
                    if let Some(color) = color_from(&values) {
                        if op.operator.starts_with('s') {
                            state.fill = color;
                        } else {
                            state.stroke = color;
                        }
                    }
                }

                // -- text ------------------------------------------------------
                "BT" => {
                    state.text_matrix = Matrix::IDENTITY;
                    state.line_matrix = Matrix::IDENTITY;
                }
                "ET" => {}
                "Tf" => {
                    state.font_size = n(1);
                    if let Some(PdfObject::Name(name)) = op.operands.first() {
                        state.font = self.load_font(name, resources);
                    }
                }
                "Td" => {
                    state.line_matrix = Matrix::translate(n(0), n(1)).then(&state.line_matrix);
                    state.text_matrix = state.line_matrix;
                }
                "TD" => {
                    state.leading = -n(1);
                    state.line_matrix = Matrix::translate(n(0), n(1)).then(&state.line_matrix);
                    state.text_matrix = state.line_matrix;
                }
                "Tm" => {
                    if op.operands.len() >= 6 {
                        state.line_matrix = Matrix::new(n(0), n(1), n(2), n(3), n(4), n(5));
                        state.text_matrix = state.line_matrix;
                    }
                }
                "T*" => next_line(state),
                "TL" => state.leading = n(0),
                "Tc" => state.char_spacing = n(0),
                "Tw" => state.word_spacing = n(0),
                "Tz" => state.horizontal_scale = n(0) / 100.0,
                "Ts" => state.rise = n(0),
                "Tr" => state.render_mode = op.operands.first().and_then(PdfObject::as_i64).unwrap_or(0),
                "Tj" => {
                    if let Some(bytes) = string_operand(op, 0) {
                        self.show_text(&bytes, state);
                    }
                }
                "'" => {
                    next_line(state);
                    if let Some(bytes) = string_operand(op, 0) {
                        self.show_text(&bytes, state);
                    }
                }
                "\"" => {
                    state.word_spacing = n(0);
                    state.char_spacing = n(1);
                    next_line(state);
                    if let Some(bytes) = string_operand(op, 2) {
                        self.show_text(&bytes, state);
                    }
                }
                "TJ" => {
                    if let Some(PdfObject::Array(items)) = op.operands.first() {
                        for item in items {
                            match item {
                                PdfObject::LiteralString(bytes)
                                | PdfObject::HexString(bytes) => {
                                    self.show_text(bytes, state)
                                }
                                other => {
                                    if let Some(adjust) = as_number(other) {
                                        // Kerning is expressed in 1/1000 em,
                                        // subtracted from the advance.
                                        let tx = -adjust / 1000.0
                                            * state.font_size
                                            * state.horizontal_scale;
                                        state.text_matrix = Matrix::translate(tx, 0.0)
                                            .then(&state.text_matrix);
                                    }
                                }
                            }
                        }
                    }
                }

                // -- XObjects ---------------------------------------------------
                "Do" => {
                    if let Some(PdfObject::Name(name)) = op.operands.first() {
                        self.draw_xobject(name, resources, state);
                    }
                }

                _ => {} // sh, BI/ID/EI, marked content, compatibility ops
            }
        }
        Ok(())
    }

    fn paint(
        &mut self,
        operator: &str,
        path: &mut Path,
        state: &mut GraphicsState,
        pending_clip: &mut Option<FillRule>,
    ) {
        if operator.starts_with('b') || operator == "s" {
            path.close();
        }
        let device = path.transform(&state.ctm);

        let fills = matches!(operator, "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
        let strokes = matches!(operator, "S" | "s" | "B" | "B*" | "b" | "b*");
        let rule = if operator.ends_with('*') {
            FillRule::EvenOdd
        } else {
            FillRule::NonZero
        };

        if fills && !device.is_empty() {
            self.canvas.fill_path(
                &device,
                state.fill,
                rule,
                state.fill_alpha,
                state.clip.as_deref(),
            );
        }
        if strokes && !device.is_empty() {
            // Line width is in user space; scale it into device space.
            let width = state.line_width * state.ctm.mean_scale();
            let outline = stroke_outline(&device, width);
            self.canvas.fill_path(
                &outline,
                state.stroke,
                FillRule::NonZero,
                state.stroke_alpha,
                state.clip.as_deref(),
            );
        }

        // `W` names the clip, but it only takes effect after the painting
        // operator that follows it — which is this one.
        if let Some(clip_rule) = pending_clip.take() {
            let mask = self.canvas.rasterize_mask(&device, clip_rule);
            state.clip = Some(Rc::new(match state.clip.as_deref() {
                Some(existing) => existing.intersect(&mask),
                None => mask,
            }));
        }
    }

    fn apply_ext_gstate(
        &mut self,
        op: &Operation,
        resources: &Dictionary,
        state: &mut GraphicsState,
    ) {
        let Some(PdfObject::Name(name)) = op.operands.first() else {
            return;
        };
        let Some(dict) = self.resource(resources, "ExtGState", name) else {
            return;
        };
        if let Some(value) = dict.get("ca").and_then(as_number) {
            state.fill_alpha = value.clamp(0.0, 1.0) as f32;
        }
        if let Some(value) = dict.get("CA").and_then(as_number) {
            state.stroke_alpha = value.clamp(0.0, 1.0) as f32;
        }
        if let Some(value) = dict.get("LW").and_then(as_number) {
            state.line_width = value;
        }
    }

    fn space_components(&self, op: &Operation, resources: &Dictionary) -> usize {
        match op.operands.first() {
            Some(PdfObject::Name(name)) => match name.as_str() {
                "DeviceGray" | "G" | "CalGray" | "Pattern" => 1,
                "DeviceCMYK" | "CMYK" => 4,
                "DeviceRGB" | "RGB" | "CalRGB" => 3,
                other => self
                    .resource_object(resources, "ColorSpace", other)
                    .map(|object| components_of_space(self.doc, &object))
                    .unwrap_or(3),
            },
            _ => 3,
        }
    }

    fn load_font(&mut self, name: &str, resources: &Dictionary) -> Option<Rc<RenderFont>> {
        // Cache per resource name — a page typically reuses a handful.
        if let Some(font) = self.fonts.get(name) {
            return Some(Rc::clone(font));
        }
        let dict = self.resource(resources, "Font", name)?;
        let font = Rc::new(RenderFont::load(self.doc, &dict));
        self.fonts.insert(name.to_owned(), Rc::clone(&font));
        Some(font)
    }

    fn show_text(&mut self, bytes: &[u8], state: &mut GraphicsState) {
        let Some(font) = state.font.clone() else {
            return;
        };
        // Render mode 3 (and 7) is invisible text — the OCR layer sitting
        // under a scanned image. Advance through it without drawing.
        let invisible = state.render_mode == 3 || state.render_mode == 7;
        let units_per_em = font.units_per_em();

        if !invisible {
            if font.is_substituted() {
                self.warn("some text uses a substitute font: the document did not embed its own");
            } else if !font.can_draw_glyphs() {
                self.warn("some text could not be drawn: no usable font outlines");
            }
        }

        for code in font.text.codes(bytes) {
            let width = font.advance_width(code) / 1000.0;

            if !invisible && font.can_draw_glyphs() {
                if let Some(outline) = font.outline(code) {
                    // Glyph space -> text space -> user space -> device.
                    let scale = Matrix::scale(
                        state.font_size * state.horizontal_scale / units_per_em,
                        state.font_size / units_per_em,
                    );
                    let offset = Matrix::translate(0.0, state.rise);
                    let trm = scale
                        .then(&offset)
                        .then(&state.text_matrix)
                        .then(&state.ctm);
                    let device = outline.transform(&trm);
                    let color = if state.render_mode == 1 || state.render_mode == 5 {
                        state.stroke
                    } else {
                        state.fill
                    };
                    self.canvas.fill_path(
                        &device,
                        color,
                        FillRule::NonZero,
                        state.fill_alpha,
                        state.clip.as_deref(),
                    );
                }
            }

            let word = if font.text.is_space_code(code) {
                state.word_spacing
            } else {
                0.0
            };
            let advance =
                (width * state.font_size + state.char_spacing + word) * state.horizontal_scale;
            state.text_matrix = Matrix::translate(advance, 0.0).then(&state.text_matrix);
        }
    }

    fn draw_xobject(&mut self, name: &str, resources: &Dictionary, state: &mut GraphicsState) {
        if self.depth > 12 {
            return; // guard against recursive form XObjects
        }
        let Some(object) = self.resource_object(resources, "XObject", name) else {
            return;
        };
        let PdfObject::Stream(stream) = object else {
            return;
        };
        let subtype = stream
            .dictionary
            .get("Subtype")
            .and_then(PdfObject::as_name)
            .unwrap_or("");

        match subtype {
            "Image" => self.draw_image(&stream, state),
            "Form" => {
                let mut inner = state.clone();
                if let Some(PdfObject::Array(items)) = stream
                    .dictionary
                    .get("Matrix")
                    .map(|o| self.doc.resolve_value(o))
                {
                    let values: Vec<f64> = items.iter().filter_map(as_number).collect();
                    if values.len() >= 6 {
                        let m = Matrix::new(
                            values[0], values[1], values[2], values[3], values[4], values[5],
                        );
                        inner.ctm = m.then(&inner.ctm);
                    }
                }
                // /BBox clips the form's contents.
                if let Some(bbox) = array_rect(self.doc, &stream.dictionary, "BBox") {
                    let mut clip_path = Path::new();
                    clip_path.rect(
                        bbox.0.min(bbox.2),
                        bbox.1.min(bbox.3),
                        (bbox.2 - bbox.0).abs(),
                        (bbox.3 - bbox.1).abs(),
                    );
                    let device = clip_path.transform(&inner.ctm);
                    let mask = self.canvas.rasterize_mask(&device, FillRule::NonZero);
                    inner.clip = Some(Rc::new(match inner.clip.as_deref() {
                        Some(existing) => existing.intersect(&mask),
                        None => mask,
                    }));
                }

                let inner_resources = stream
                    .dictionary
                    .get("Resources")
                    .map(|o| self.doc.resolve_value(o))
                    .and_then(|o| self.doc.resolve_dict(&o).cloned())
                    .unwrap_or_else(|| resources.clone());

                if let Ok(data) = self.doc.stream_data(&stream) {
                    self.depth += 1;
                    let _ = self.run(&data, &inner_resources, &mut inner);
                    self.depth -= 1;
                }
            }
            _ => {}
        }
    }

    /// Paint an image into the unit square mapped by the CTM.
    ///
    /// Walks device pixels and inverse-maps each one into image space, which
    /// handles rotation and skew for free and never allocates a resampled
    /// intermediate.
    fn draw_image(&mut self, stream: &pdf_core::stream::PdfStream, state: &GraphicsState) {
        let Some(image) = decode_image(self.doc, stream) else {
            let codec = image_codec_name(self.doc, stream);
            self.warn(format!("image skipped: {codec} is not supported"));
            return;
        };
        let Some(inverse) = state.ctm.invert() else {
            return;
        };

        // Device-space bounds of the transformed unit square.
        let corners = [
            state.ctm.apply(0.0, 0.0),
            state.ctm.apply(1.0, 0.0),
            state.ctm.apply(0.0, 1.0),
            state.ctm.apply(1.0, 1.0),
        ];
        let min_x = corners.iter().map(|c| c.0).fold(f64::MAX, f64::min);
        let max_x = corners.iter().map(|c| c.0).fold(f64::MIN, f64::max);
        let min_y = corners.iter().map(|c| c.1).fold(f64::MAX, f64::min);
        let max_y = corners.iter().map(|c| c.1).fold(f64::MIN, f64::max);

        let x0 = min_x.floor().max(0.0) as usize;
        let y0 = min_y.floor().max(0.0) as usize;
        let x1 = (max_x.ceil().min(self.canvas.width as f64)).max(0.0) as usize;
        let y1 = (max_y.ceil().min(self.canvas.height as f64)).max(0.0) as usize;

        let clip = state.clip.clone();
        for py in y0..y1 {
            for px in x0..x1 {
                // Sample at the pixel centre.
                let (u, v) = inverse.apply(px as f64 + 0.5, py as f64 + 0.5);
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                // Image space runs top-down, the unit square bottom-up.
                let sx = (u * image.width as f64) as usize;
                let sy = ((1.0 - v) * image.height as f64) as usize;
                let Some((color, alpha)) = image.sample(sx, sy) else {
                    continue;
                };
                let color = if image.is_stencil { state.fill } else { color };
                self.canvas
                    .blend(px, py, color, alpha * state.fill_alpha, clip.as_deref());
            }
        }
    }

    fn resource(&self, resources: &Dictionary, category: &str, name: &str) -> Option<Dictionary> {
        let object = self.resource_object(resources, category, name)?;
        self.doc.resolve_dict(&object).cloned()
    }

    fn resource_object(
        &self,
        resources: &Dictionary,
        category: &str,
        name: &str,
    ) -> Option<PdfObject> {
        let group = self.doc.resolve_value(resources.get(category)?);
        let dict = self.doc.resolve_dict(&group)?;
        Some(self.doc.resolve_value(dict.get(name)?))
    }
}

fn next_line(state: &mut GraphicsState) {
    state.line_matrix = Matrix::translate(0.0, -state.leading).then(&state.line_matrix);
    state.text_matrix = state.line_matrix;
}

fn components_of_space(doc: &PdfDocument, object: &PdfObject) -> usize {
    match object {
        PdfObject::Name(name) => match name.as_str() {
            "DeviceGray" | "CalGray" | "G" => 1,
            "DeviceCMYK" | "CMYK" => 4,
            _ => 3,
        },
        PdfObject::Array(items) => match items.first().and_then(PdfObject::as_name) {
            Some("ICCBased") => items
                .get(1)
                .map(|o| doc.resolve_value(o))
                .and_then(|o| match o {
                    PdfObject::Stream(s) => s.dictionary.get("N").and_then(PdfObject::as_i64),
                    _ => None,
                })
                .unwrap_or(3) as usize,
            Some("Indexed") | Some("I") | Some("Separation") => 1,
            Some("DeviceN") => items
                .get(1)
                .map(|o| doc.resolve_value(o))
                .and_then(|o| match o {
                    PdfObject::Array(names) => Some(names.len()),
                    _ => None,
                })
                .unwrap_or(1),
            Some("DeviceCMYK") => 4,
            Some("DeviceGray") | Some("CalGray") => 1,
            _ => 3,
        },
        _ => 3,
    }
}

/// Interpret 1/3/4 raw colour components as gray/RGB/CMYK.
fn color_from(values: &[f64]) -> Option<Rgb> {
    match values.len() {
        1 => Some(Rgb::gray(values[0] as f32)),
        3 => Some(Rgb::new(values[0] as f32, values[1] as f32, values[2] as f32)),
        4 => Some(Rgb::from_cmyk(
            values[0] as f32,
            values[1] as f32,
            values[2] as f32,
            values[3] as f32,
        )),
        _ => None,
    }
}

/// The image codec a stream asks for, for a warning the user can act on.
fn image_codec_name(doc: &PdfDocument, stream: &pdf_core::stream::PdfStream) -> String {
    let entry = stream
        .dictionary
        .get("Filter")
        .or_else(|| stream.dictionary.get("F"))
        .map(|o| doc.resolve_value(o));
    let names: Vec<String> = match entry {
        Some(PdfObject::Name(name)) => vec![name],
        Some(PdfObject::Array(items)) => items
            .iter()
            .map(|o| doc.resolve_value(o))
            .filter_map(|o| o.as_name().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .rev()
        .find(|n| {
            matches!(
                n.as_str(),
                "JPXDecode" | "JBIG2Decode" | "CCITTFaxDecode" | "CCF" | "DCTDecode" | "DCT"
            )
        })
        .unwrap_or_else(|| "this image format".to_owned())
}

/// A page's concatenated content streams, plus what it cost to get them.
struct PageContent {
    data: Vec<u8>,
    /// How many content streams were present, and how many would not decode.
    streams: usize,
    failed: usize,
    /// Why the first failure happened, for the error message.
    first_error: Option<String>,
}

fn page_content(doc: &PdfDocument, page: &Dictionary) -> Result<PageContent> {
    let mut content = PageContent {
        data: Vec::new(),
        streams: 0,
        failed: 0,
        first_error: None,
    };
    let Some(entry) = page.get("Contents") else {
        return Ok(content);
    };

    let take = |content: &mut PageContent, stream: &pdf_core::stream::PdfStream| {
        content.streams += 1;
        match doc.stream_data(stream) {
            Ok(bytes) => {
                content.data.extend_from_slice(&bytes);
                content.data.push(b'\n');
            }
            Err(err) => {
                content.failed += 1;
                if content.first_error.is_none() {
                    content.first_error = Some(err.to_string());
                }
            }
        }
    };

    match doc.resolve_value(entry) {
        PdfObject::Stream(stream) => take(&mut content, &stream),
        PdfObject::Array(items) => {
            for item in items {
                if let PdfObject::Stream(stream) = doc.resolve_value(&item) {
                    take(&mut content, &stream);
                }
            }
        }
        _ => {}
    }
    Ok(content)
}

fn rect(doc: &PdfDocument, page: &Dictionary, key: &str) -> Option<(f64, f64, f64, f64)> {
    array_rect(doc, page, key)
}

fn array_rect(doc: &PdfDocument, dict: &Dictionary, key: &str) -> Option<(f64, f64, f64, f64)> {
    let PdfObject::Array(items) = doc.resolve_value(dict.get(key)?) else {
        return None;
    };
    let values: Vec<f64> = items
        .iter()
        .map(|o| doc.resolve_value(o))
        .filter_map(|o| as_number(&o))
        .collect();
    if values.len() < 4 {
        return None;
    }
    Some((
        values[0].min(values[2]),
        values[1].min(values[3]),
        values[0].max(values[2]),
        values[1].max(values[3]),
    ))
}

fn number(op: &Operation, index: usize) -> f64 {
    op.operands.get(index).and_then(as_number).unwrap_or(0.0)
}

fn as_number(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(v) => Some(*v as f64),
        PdfObject::Real(v) => Some(*v),
        _ => None,
    }
}

fn string_operand(op: &Operation, index: usize) -> Option<Vec<u8>> {
    match op.operands.get(index)? {
        PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

/// Unused today, but keeps the `ObjectId` import meaningful for callers that
/// want to render a specific page object rather than an index.
pub fn page_id_at(doc: &PdfDocument, index: usize) -> Option<ObjectId> {
    doc.collect_page_ids()?.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_core::stream::PdfStream;

    #[test]
    fn tiny_fit_boxes_are_respected() {
        let doc = doc_with_content("", [0, 0, 200, 100]);
        let page = render_page(
            &doc,
            0,
            RenderOptions {
                size: RenderSize::FitBox {
                    width: 1,
                    height: 1,
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((page.width, page.height), (1, 1));
    }

    #[test]
    fn extreme_aspect_ratios_respect_the_pixel_budget() {
        for media in [[0, 0, 100_000, 1], [0, 0, 1, 100_000]] {
            let doc = doc_with_content("", media);
            let page = render_page(
                &doc,
                0,
                RenderOptions {
                    max_pixels: 16,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(page.width > 0 && page.height > 0);
            assert!(page.width as usize * page.height as usize <= 16);
            assert_eq!(
                page.pixels.len(),
                page.width as usize * page.height as usize * 4
            );
        }
    }

    #[test]
    fn huge_page_dimensions_cannot_overflow_pixel_accounting() {
        let doc = doc_with_content("", [0, 0, i64::MAX, i64::MAX]);
        let page = render_page(
            &doc,
            0,
            RenderOptions {
                max_pixels: 16,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((page.width, page.height), (4, 4));
    }

    #[test]
    fn rounding_and_large_scales_cannot_exceed_pixel_budgets() {
        for media in [[0, 0, 1, 1], [0, 0, 13, 29], [0, 0, 100_000, 1]] {
            let doc = doc_with_content("", media);
            for max_pixels in [1, 7, 17, 997] {
                for scale in [f64::MIN_POSITIVE, 1.0, f64::MAX] {
                    let page = render_page(
                        &doc,
                        0,
                        RenderOptions {
                            size: RenderSize::Scale(scale),
                            max_pixels,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    assert!(page.width > 0 && page.height > 0);
                    assert!(page.width as usize <= max_pixels / page.height as usize);
                    assert_eq!(
                        page.pixels.len(),
                        page.width as usize * page.height as usize * 4
                    );
                }
            }
        }
    }

    #[test]
    fn non_finite_page_extents_are_errors() {
        let mut doc = doc_with_content("", [0, 0, 200, 100]);
        let id = doc.collect_page_ids().unwrap()[0];
        let mut dict = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        dict.insert(
            "MediaBox".into(),
            PdfObject::Array(
                [-f64::MAX, 0.0, f64::MAX, 100.0]
                    .into_iter()
                    .map(PdfObject::Real)
                    .collect(),
            ),
        );
        doc.set_object(id, PdfObject::Dictionary(dict));
        assert!(page_size_points(&doc, 0).is_err());
        assert!(render_page(&doc, 0, RenderOptions::default()).is_err());
    }

    #[test]
    fn invalid_render_sizes_and_empty_budgets_are_errors() {
        let doc = doc_with_content("", [0, 0, 2, 2]);
        for size in [
            RenderSize::Scale(0.0),
            RenderSize::Scale(-1.0),
            RenderSize::Scale(f64::NAN),
            RenderSize::Scale(f64::INFINITY),
            RenderSize::FitBox {
                width: 0,
                height: 10,
            },
            RenderSize::FitBox {
                width: 10,
                height: 0,
            },
        ] {
            assert!(render_page(
                &doc,
                0,
                RenderOptions {
                    size,
                    ..Default::default()
                }
            )
            .is_err());
        }
        assert!(render_page(
            &doc,
            0,
            RenderOptions {
                max_pixels: 0,
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn page_size_resolves_indirect_rotation_like_rendering() {
        let mut doc = doc_with_content("", [0, 0, 200, 100]);
        let rotation = doc.add_object(PdfObject::Integer(90));
        let id = doc.collect_page_ids().unwrap()[0];
        let mut dict = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        dict.insert("Rotate".into(), PdfObject::Reference(rotation));
        doc.set_object(id, PdfObject::Dictionary(dict));
        assert_eq!(page_size_points(&doc, 0).unwrap(), (100.0, 200.0));
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!((page.width, page.height), (100, 200));
    }

    /// Build a one-page document whose content stream is `content`.
    fn doc_with_content(content: &str, media: [i64; 4]) -> PdfDocument {
        let mut doc = PdfDocument::new_empty("1.7");
        let mut stream_dict = Dictionary::new();
        stream_dict.insert("Length".into(), PdfObject::Integer(content.len() as i64));
        let content_id = doc.add_object(PdfObject::Stream(PdfStream::new(
            stream_dict,
            content.as_bytes().to_vec(),
        )));

        let pages_id = ObjectId::new(90, 0);
        let mut page = Dictionary::new();
        page.insert("Type".into(), PdfObject::Name("Page".into()));
        page.insert("Parent".into(), PdfObject::Reference(pages_id));
        page.insert(
            "MediaBox".into(),
            PdfObject::Array(media.iter().map(|v| PdfObject::Integer(*v)).collect()),
        );
        page.insert("Resources".into(), PdfObject::Dictionary(Dictionary::new()));
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

    fn pixel(page: &RenderedPage, x: u32, y: u32) -> (u8, u8, u8) {
        let offset = ((y * page.width + x) * 4) as usize;
        (
            page.pixels[offset],
            page.pixels[offset + 1],
            page.pixels[offset + 2],
        )
    }

    #[test]
    fn renders_at_the_requested_size() {
        let doc = doc_with_content("", [0, 0, 200, 100]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!((page.width, page.height), (200, 100));
        assert_eq!(page.pixels.len(), 200 * 100 * 4);

        let doubled = render_page(
            &doc,
            0,
            RenderOptions {
                size: RenderSize::Scale(2.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((doubled.width, doubled.height), (400, 200));
    }

    #[test]
    fn fit_box_preserves_aspect_ratio() {
        let doc = doc_with_content("", [0, 0, 200, 100]);
        let page = render_page(
            &doc,
            0,
            RenderOptions {
                size: RenderSize::FitBox {
                    width: 100,
                    height: 100,
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((page.width, page.height), (100, 50));
    }

    #[test]
    fn empty_page_is_the_background_colour() {
        let doc = doc_with_content("", [0, 0, 50, 50]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 25, 25), (255, 255, 255));
    }

    #[test]
    fn fills_a_rectangle_in_user_space() {
        // PDF y-up: this rect sits in the *lower* half of the page, which is
        // the *bottom* of the image once flipped.
        let doc = doc_with_content("0 0 0 rg 0 0 100 50 re f", [0, 0, 100, 100]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 50, 75), (0, 0, 0), "bottom half filled");
        assert_eq!(pixel(&page, 50, 25), (255, 255, 255), "top half clear");
    }

    #[test]
    fn honours_rgb_and_cmyk_fill_colours() {
        let doc = doc_with_content("1 0 0 rg 0 0 100 100 re f", [0, 0, 100, 100]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 50, 50), (255, 0, 0));

        let cmyk = doc_with_content("1 0 0 0 k 0 0 100 100 re f", [0, 0, 100, 100]);
        let page = render_page(&cmyk, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 50, 50), (0, 255, 255), "cyan");
    }

    #[test]
    fn cm_transforms_the_path() {
        // Translate a unit square to (20,20) and scale it by 10.
        let doc = doc_with_content(
            "0 0 0 rg q 10 0 0 10 20 20 cm 0 0 1 1 re f Q",
            [0, 0, 100, 100],
        );
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        // User (25,25) -> device y = 100 - 25 = 75.
        assert_eq!(pixel(&page, 25, 75), (0, 0, 0));
        assert_eq!(pixel(&page, 5, 95), (255, 255, 255));
    }

    #[test]
    fn q_restores_the_previous_state() {
        let doc = doc_with_content("q 1 0 0 rg Q 0 0 100 100 re f", [0, 0, 100, 100]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        // The red set inside q…Q must not survive; default fill is black.
        assert_eq!(pixel(&page, 50, 50), (0, 0, 0));
    }

    #[test]
    fn clipping_path_limits_later_painting() {
        let doc = doc_with_content(
            "0 0 50 100 re W n 0 0 0 rg 0 0 100 100 re f",
            [0, 0, 100, 100],
        );
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 25, 50), (0, 0, 0), "inside clip");
        assert_eq!(pixel(&page, 75, 50), (255, 255, 255), "outside clip");
    }

    #[test]
    fn strokes_are_painted() {
        let doc = doc_with_content("0 0 0 RG 4 w 10 50 m 90 50 l S", [0, 0, 100, 100]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 50, 50), (0, 0, 0), "on the line");
        assert_eq!(pixel(&page, 50, 20), (255, 255, 255), "away from it");
    }

    #[test]
    fn ext_gstate_alpha_is_applied() {
        let mut doc = doc_with_content("/GS0 gs 0 0 0 rg 0 0 100 100 re f", [0, 0, 100, 100]);
        // Attach an ExtGState with ca 0.5 to the page's resources.
        let mut gs = Dictionary::new();
        gs.insert("ca".into(), PdfObject::Real(0.5));
        let mut group = Dictionary::new();
        group.insert("GS0".into(), PdfObject::Dictionary(gs));
        let mut ext = Dictionary::new();
        ext.insert("ExtGState".into(), PdfObject::Dictionary(group));

        let page_id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
        page.insert("Resources".into(), PdfObject::Dictionary(ext));
        doc.set_object(page_id, PdfObject::Dictionary(page));

        let rendered = render_page(&doc, 0, RenderOptions::default()).unwrap();
        let (r, _, _) = pixel(&rendered, 50, 50);
        assert!((r as i32 - 128).abs() <= 3, "expected ~50% grey, got {r}");
    }

    #[test]
    fn rotate_90_swaps_the_output_dimensions() {
        let mut doc = doc_with_content("", [0, 0, 200, 100]);
        let page_id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
        page.insert("Rotate".into(), PdfObject::Integer(90));
        doc.set_object(page_id, PdfObject::Dictionary(page));

        let rendered = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!((rendered.width, rendered.height), (100, 200));
    }

    #[test]
    fn max_pixels_caps_absurd_page_sizes() {
        let doc = doc_with_content("", [0, 0, 20000, 20000]);
        let page = render_page(
            &doc,
            0,
            RenderOptions {
                size: RenderSize::Scale(4.0),
                max_pixels: 1_000_000,
                ..Default::default()
            },
        )
        .unwrap();
        assert!((page.width as usize) * (page.height as usize) <= 1_000_000);
    }

    #[test]
    fn out_of_range_page_is_an_error() {
        let doc = doc_with_content("", [0, 0, 100, 100]);
        assert!(render_page(&doc, 7, RenderOptions::default()).is_err());
    }

    #[test]
    fn malformed_content_still_yields_a_page() {
        let doc = doc_with_content("this is not ( valid pdf content", [0, 0, 60, 60]);
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!((page.width, page.height), (60, 60));
    }

    #[test]
    fn renders_the_repository_fixtures() {
        for fixture in [
            &include_bytes!("../../../fixtures/simple.pdf")[..],
            &include_bytes!("../../../fixtures/two_pages.pdf")[..],
        ] {
            let doc = PdfDocument::from_bytes(fixture).unwrap();
            let count = doc.page_count().unwrap_or(0);
            assert!(count >= 1);
            for index in 0..count as usize {
                let page = render_page(
                    &doc,
                    index,
                    RenderOptions {
                        size: RenderSize::FitBox {
                            width: 200,
                            height: 200,
                        },
                        ..Default::default()
                    },
                )
                .unwrap();
                assert!(page.width > 0 && page.height > 0);
                assert_eq!(page.pixels.len(), (page.width * page.height * 4) as usize);
            }
        }
    }

    #[test]
    fn page_size_points_reports_the_media_box() {
        let doc = doc_with_content("", [0, 0, 612, 792]);
        assert_eq!(page_size_points(&doc, 0).unwrap(), (612.0, 792.0));
    }
}
