//! Content-stream execution: turns a page into pixels.
//!
//! Reuses `pdf_text::content_stream::parse_content` for the operator stream
//! and `pdf_text::font` for metrics, so the renderer and the text extractor
//! agree on how a page is structured.
//!
//! Supported: the graphics state stack, path construction and painting
//! (fill/stroke/even-odd), arbitrary clipping paths, DeviceGray/RGB/CMYK plus
//! ICCBased/Indexed/Separation colour, constant alpha from `/ExtGState`,
//! image XObjects (JPEG, Flate, stencil masks, soft masks), form XObjects and
//! TrueType text.
//!
//! Not supported, and deliberately skipped rather than failed: shading
//! patterns (`sh`), tiling patterns, inline images (`BI…EI`), blend modes and
//! CFF/Type1 glyph outlines. Pages using those render with everything else
//! intact.

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
    /// Multiply the page's point size by this factor (1.0 = 72 dpi).
    Scale(f64),
    /// Fit inside this pixel box, preserving aspect ratio.
    FitBox { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub size: RenderSize,
    pub background: Rgb,
    /// Hard cap on output pixels, so a malformed /MediaBox cannot ask for a
    /// gigapixel buffer on a phone.
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
    let page_w = (box_rect.2 - box_rect.0).abs().max(1.0);
    let page_h = (box_rect.3 - box_rect.1).abs().max(1.0);

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

    let scale = match options.size {
        RenderSize::Scale(s) => s.max(0.01),
        RenderSize::FitBox { width, height } => {
            (width as f64 / display_w).min(height as f64 / display_h).max(0.01)
        }
    };
    let mut out_w = (display_w * scale).round().max(1.0) as usize;
    let mut out_h = (display_h * scale).round().max(1.0) as usize;
    if out_w * out_h > options.max_pixels {
        let shrink = (options.max_pixels as f64 / (out_w * out_h) as f64).sqrt();
        out_w = ((out_w as f64 * shrink).round() as usize).max(1);
        out_h = ((out_h as f64 * shrink).round() as usize).max(1);
    }

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
    let device_scale = Matrix::scale(
        out_w as f64 / display_w,
        out_h as f64 / display_h,
    );
    let base_ctm = flip.then(&rotation).then(&device_scale);

    let resources = page
        .get("Resources")
        .map(|o| doc.resolve_value(o))
        .and_then(|o| doc.resolve_dict(&o).cloned())
        .unwrap_or_default();
    let content = page_content(doc, &page)?;

    let mut renderer = Renderer {
        doc,
        canvas: &mut canvas,
        fonts: HashMap::new(),
        depth: 0,
    };
    let mut state = GraphicsState::new(base_ctm);
    // A failed content stream should still yield the page background rather
    // than an error — damaged files are common and a blank page beats none.
    let _ = renderer.run(&content, &resources, &mut state);

    Ok(RenderedPage {
        width: out_w as u32,
        height: out_h as u32,
        pixels: canvas.pixels,
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
    let (w, h) = ((r.2 - r.0).abs().max(1.0), (r.3 - r.1).abs().max(1.0));
    let rotate = page
        .get("Rotate")
        .and_then(PdfObject::as_i64)
        .unwrap_or(0)
        .rem_euclid(360);
    Ok(if rotate == 90 || rotate == 270 {
        (h, w)
    } else {
        (w, h)
    })
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
}

impl Renderer<'_> {
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

        for code in font.text.codes(bytes) {
            let width = font.text.width(code) / 1000.0;

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

fn page_content(doc: &PdfDocument, page: &Dictionary) -> Result<Vec<u8>> {
    let Some(entry) = page.get("Contents") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    match doc.resolve_value(entry) {
        PdfObject::Stream(stream) => out = doc.stream_data(&stream).unwrap_or_default(),
        PdfObject::Array(items) => {
            for item in items {
                if let PdfObject::Stream(stream) = doc.resolve_value(&item) {
                    out.extend_from_slice(&doc.stream_data(&stream).unwrap_or_default());
                    out.push(b'\n');
                }
            }
        }
        _ => {}
    }
    Ok(out)
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
        let doc = doc_with_content(
            "q 1 0 0 rg Q 0 0 100 100 re f",
            [0, 0, 100, 100],
        );
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
        let doc = doc_with_content(
            "0 0 0 RG 4 w 10 50 m 90 50 l S",
            [0, 0, 100, 100],
        );
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        assert_eq!(pixel(&page, 50, 50), (0, 0, 0), "on the line");
        assert_eq!(pixel(&page, 50, 20), (255, 255, 255), "away from it");
    }

    #[test]
    fn ext_gstate_alpha_is_applied() {
        let mut doc = doc_with_content(
            "/GS0 gs 0 0 0 rg 0 0 100 100 re f",
            [0, 0, 100, 100],
        );
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
