//! Content-stream execution: turns a page into pixels.
//!
//! Reuses `pdf_text::content_stream::parse_content` for the operator stream
//! and `pdf_text::font` for metrics, so the renderer and the text extractor
//! agree on how a page is structured.
//!
//! Supported: the graphics state stack, path construction and painting
//! (fill/stroke/even-odd), arbitrary clipping paths, DeviceGray/RGB/CMYK plus
//! ICCBased/Indexed/Separation colour, constant alpha from `/ExtGState`,
//! image XObjects (JPEG, JPEG2000, JBIG2, CCITT, Flate, LZW and masks),
//! inline images, transparency groups, standard blend modes, annotation
//! appearances/fallbacks, all standard shadings, tiling patterns and TrueType/CFF
//! text including stroke and clipping modes.
//!
//! Text whose font the document did not embed — the standard 14, or a program
//! in a format this renderer cannot parse — is drawn in a substitute face; see
//! [`crate::font::fallback`]. It is drawn, not skipped, because a page of
//! invisible text is indistinguishable from a broken file.
//!
//! Damaged input, resource limits, unsupported colour-space combinations and
//! approximate missing-appearance fallbacks are reported in
//! [`RenderedPage::warnings`]. Rendering a signature appearance does not verify it.

#[path = "page_annotations.rs"]
mod annotations;
#[path = "page_bands.rs"]
mod bands;
#[path = "page_forms.rs"]
mod forms;
#[path = "page_images.rs"]
mod images;
#[path = "page_limits.rs"]
mod limits;
#[path = "page_optional_content.rs"]
mod optional_content;
#[path = "page_paints.rs"]
mod paints;
#[path = "page_strokes.rs"]
mod strokes;
#[path = "page_text.rs"]
mod text;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_ops::page_tree::effective_page_dict;
use pdf_text::content_stream::{parse_content, Content, ContentOp, Operand};

use crate::canvas::{stroke_outline, BlendMode, Canvas, ClipMask, Rgb};
use crate::font::RenderFont;
use crate::geom::{FillRule, Matrix, Path};
use crate::image::decode_image_within;
use paints::ColorFunction;

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
    /// Draw only this part of the page as `size` lays it out. A viewer zoomed
    /// far into a page needs sharp pixels for what is on screen, not a
    /// full-page bitmap of hundreds of megapixels; `max_pixels` then bounds
    /// the region rather than the page. Clipped to the page.
    pub region: Option<PixelRegion>,
    /// Threads to draw with, the caller's among them: the page is cut into
    /// horizontal bands drawn side by side. 0 uses the device's cores (up
    /// to 4) for renders large enough to gain from them; 1 draws on the
    /// calling thread alone.
    pub threads: usize,
    /// How long drawing may take. When it runs out, the render stops where
    /// it is and returns what it drew, with a "page too complex" warning,
    /// as it does when the page needs more work or memory than the
    /// renderer allows. `None` draws for as long as the page takes, within
    /// those limits.
    pub time_limit: Option<Duration>,
}

/// A rectangle of a rendered page, in its output pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            size: RenderSize::Scale(1.0),
            background: Rgb::new(1.0, 1.0, 1.0),
            max_pixels: 32_000_000,
            region: None,
            threads: 0,
            time_limit: Some(DEFAULT_TIME_LIMIT),
        }
    }
}

/// About twenty seconds of drawing a page: past that a phone's reader has
/// long given up waiting, and the page is too complex to show in full.
pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(20);

/// Stops a render from another thread. Once [`RenderCancel::cancel`] is
/// called, a render given this flag stops within a few operators and
/// returns what it drew, with a warning that it was cancelled. Clones share
/// the flag.
#[derive(Debug, Clone, Default)]
pub struct RenderCancel(Arc<AtomicBool>);

impl RenderCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
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
    render_page_cancellable(doc, index, options, &RenderCancel::default())
}

/// [`render_page`], stopping early once `cancel` is cancelled.
pub fn render_page_cancellable(
    doc: &PdfDocument,
    index: usize,
    options: RenderOptions,
    cancel: &RenderCancel,
) -> Result<RenderedPage> {
    let started = Instant::now();
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

    let (page_px_w, page_px_h) = output_dimensions(display_w, display_h, options)?;
    let region = match options.region {
        None => PixelRegion {
            x: 0,
            y: 0,
            width: page_px_w as u32,
            height: page_px_h as u32,
        },
        Some(region) => clip_region(region, page_px_w, page_px_h, options.max_pixels)?,
    };
    let (out_w, out_h) = (region.width as usize, region.height as usize);

    // PDF user space is y-up with the origin at the crop box's lower-left;
    // the canvas is y-down from the top-left. Translate, flip, then rotate.
    let flip = Matrix::new(1.0, 0.0, 0.0, -1.0, -box_rect.0, box_rect.3);
    let rotation = match rotate {
        90 => Matrix::new(0.0, 1.0, -1.0, 0.0, page_h, 0.0),
        180 => Matrix::new(-1.0, 0.0, 0.0, -1.0, page_w, page_h),
        270 => Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, page_w),
        _ => Matrix::IDENTITY,
    };
    let device_scale = Matrix::scale(page_px_w as f64 / display_w, page_px_h as f64 / display_h);
    // A region, or a band of one, is the same page shifted so its corner is
    // the origin.
    let page_ctm = flip.then(&rotation).then(&device_scale);

    let resources = page
        .get("Resources")
        .map(|o| doc.resolve_value(o))
        .and_then(|o| doc.resolve_dict(&o).cloned())
        .unwrap_or_default();
    // A document the app keeps open keeps what its renders derive from it:
    // the next render of this page, zoomed in further, parses nothing and
    // decodes no image again. Otherwise the bands of this render share it.
    let kept = doc.derived.get_or_init(bands::Shared::default);
    let fresh;
    let shared = match &kept {
        Some(shared) => shared.as_ref(),
        None => {
            fresh = bands::Shared::default();
            &fresh
        }
    };
    let parsed = shared
        .page(page_id, || Some(ParsedPage::load(doc, &page)))
        .ok_or_else(|| PdfError::Structure("page content could not be read".into()))?;

    // Every content stream failed to decode, so there is nothing to draw and
    // no honest way to call the result a render. Returning white pixels here
    // is what made unsupported filters look like empty documents; the caller
    // needs the reason so it can say what actually went wrong.
    if parsed.streams > 0 && parsed.failed == parsed.streams {
        let reason = parsed
            .first_error
            .clone()
            .unwrap_or_else(|| "content stream could not be decoded".to_owned());
        return Err(PdfError::Filter(format!(
            "page {} has no decodable content: {reason}",
            index + 1
        )));
    }

    let budget = limits::RenderBudget::new(
        options
            .time_limit
            .and_then(|limit| started.checked_add(limit)),
        cancel.clone(),
    );
    let frame = bands::Frame {
        doc,
        page: &page,
        resources: &resources,
        content: &parsed.content,
        recent: bands::Recent::default(),
        page_ctm,
        background: options.background,
        shared,
        budget: &budget,
    };
    let (pixels, mut warnings) = frame.draw_region(region, bands::worker_count(options.threads));
    if parsed.failed > 0 {
        warnings.push(format!(
            "{} of {} content streams could not be decoded",
            parsed.failed, parsed.streams
        ));
    }
    warnings.sort();
    warnings.dedup();
    // That the render stopped part way matters most to a reader, and a
    // viewer may show only the first warning: it goes first.
    warnings.sort_by_key(|warning| !warning.starts_with(limits::STOPPED));

    Ok(RenderedPage {
        width: out_w as u32,
        height: out_h as u32,
        pixels,
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
    // When only a region is drawn, only the region is allocated, so the
    // caller's budget applies to it (see `clip_region`) and not to the page.
    let caller_budget = if options.region.is_some() {
        usize::MAX
    } else {
        options.max_pixels
    };
    let budget = caller_budget.min(isize::MAX as usize / 4 - 2);
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

/// `region` clipped to a page of `page_w` x `page_h` pixels, within the pixel
/// budget. A region that misses the page entirely is an error rather than an
/// empty bitmap, so a caller with stale geometry finds out.
fn clip_region(
    region: PixelRegion,
    page_w: usize,
    page_h: usize,
    max_pixels: usize,
) -> Result<PixelRegion> {
    let (x, y) = (region.x as usize, region.y as usize);
    if region.width == 0 || region.height == 0 || x >= page_w || y >= page_h {
        return Err(PdfError::Structure(
            "render region must overlap the page".into(),
        ));
    }
    let width = (region.width as usize).min(page_w - x);
    let height = (region.height as usize).min(page_h - y);
    let budget = max_pixels.min(isize::MAX as usize / 4 - 2);
    if budget == 0 || width > budget / height {
        return Err(PdfError::Structure(
            "render region exceeds the pixel budget".into(),
        ));
    }
    Ok(PixelRegion {
        x: region.x,
        y: region.y,
        width: width as u32,
        height: height as u32,
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
    fill_pattern: Option<String>,
    stroke_pattern: Option<String>,
    fill_pattern_space: bool,
    stroke_pattern_space: bool,
    pattern_ctm: Matrix,
    fill_components: usize,
    stroke_components: usize,
    /// Tint transform for a Separation/DeviceN fill space, when the current
    /// one has it. Without it a tint is read as a colour and the painting
    /// comes out inverted — `1 scn` in /Separation /Black means full ink.
    fill_tint: Option<Rc<TintTransform>>,
    stroke_tint: Option<Rc<TintTransform>>,
    /// Set by `d1` inside a Type3 glyph: the glyph is a stencil painted in the
    /// text's colour, so its own colour operators are ignored. Inherited by
    /// anything the glyph draws, such as a form.
    color_locked: bool,
    line_width: f64,
    line_cap: i64,
    line_join: i64,
    miter_limit: f64,
    dash: Vec<f64>,
    dash_phase: f64,
    fill_alpha: f32,
    stroke_alpha: f32,
    blend_mode: BlendMode,
    soft_mask: ClipMask,
    alpha_is_shape: bool,
    clip: ClipMask,
    /// The clip's bounding box in pixels of the whole rendered page, the same
    /// in every band of it, where `clip` only covers the band.
    clip_bounds: Option<(f64, f64, f64, f64)>,
    font: Option<Arc<RenderFont>>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    horizontal_scale: f64,
    leading: f64,
    rise: f64,
    render_mode: i64,
    text_knockout: bool,
    text_matrix: Matrix,
    line_matrix: Matrix,
}

impl GraphicsState {
    /// Filling in `/Separation /None` is defined to make no marks at all.
    fn fill_marks_nothing(&self) -> bool {
        self.fill_tint
            .as_ref()
            .is_some_and(|t| t.function.is_none())
    }

    fn stroke_marks_nothing(&self) -> bool {
        self.stroke_tint
            .as_ref()
            .is_some_and(|t| t.function.is_none())
    }

    fn new(ctm: Matrix) -> Self {
        Self {
            ctm,
            fill: Rgb::BLACK,
            stroke: Rgb::BLACK,
            fill_pattern: None,
            stroke_pattern: None,
            fill_pattern_space: false,
            stroke_pattern_space: false,
            pattern_ctm: ctm,
            fill_components: 1,
            stroke_components: 1,
            fill_tint: None,
            stroke_tint: None,
            color_locked: false,
            line_width: 1.0,
            line_cap: 0,
            line_join: 0,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_phase: 0.0,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            blend_mode: BlendMode::Normal,
            soft_mask: None,
            alpha_is_shape: false,
            clip: None,
            clip_bounds: None,
            font: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
            text_knockout: true,
            text_matrix: Matrix::IDENTITY,
            line_matrix: Matrix::IDENTITY,
        }
    }
}

/// The outer scope's name-keyed caches, held while a nested content stream
/// runs with its own resources.
struct ResourceScope {
    fonts: HashMap<String, Arc<RenderFont>>,
    tint_transforms: HashMap<String, Option<Rc<TintTransform>>>,
}

struct Renderer<'a> {
    doc: &'a PdfDocument,
    canvas: &'a mut Canvas,
    /// Fonts and images shared with the other bands of the same render.
    shared: &'a bands::Shared,
    /// The images this render used last, kept for its bands.
    recent: &'a bands::Recent,
    fonts: HashMap<String, Arc<RenderFont>>,
    /// Tint transforms by `/ColorSpace` resource name; `None` records a space
    /// that needs none, so it is not re-examined on every `cs`.
    tint_transforms: HashMap<String, Option<Rc<TintTransform>>>,
    depth: usize,
    pattern_pixels: f64,
    /// This renderer's canvas within the whole rendered page: where its
    /// band starts, and the region of the page all the bands make up.
    origin: (f64, f64),
    region: (f64, f64, f64, f64),
    temporary_bytes: usize,
    mask_allocations: Vec<std::rc::Weak<crate::canvas::Mask>>,
    temporary_limit: usize,
    render_stopped: bool,
    warnings: Vec<String>,
    visible: bool,
    /// How many Type3 glyph procedures are running. Their shapes are glyph
    /// outlines, so page-level pixel snapping stays out of them.
    type3_depth: usize,
    /// The deadline, cancel flag and picture memory shared by the render's
    /// bands, and this band's work against [`limits::MAX_WORK`],
    /// [`limits::MAX_FORM_CALLS`] and [`limits::MAX_HELD_CONTENT_BYTES`].
    budget: &'a limits::RenderBudget,
    work: u64,
    form_calls: usize,
    held_content: usize,
}

impl Renderer<'_> {
    fn configure_canvas(&mut self, state: &GraphicsState) {
        self.canvas.paint_suppressed = self.render_stopped;
        self.canvas.blend_mode = state.blend_mode;
        self.canvas.soft_mask = state.soft_mask.clone();
        self.canvas.alpha_is_shape = state.alpha_is_shape;
    }
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

    fn run_operations(
        &mut self,
        content: &Content,
        resources: &Dictionary,
        state: &mut GraphicsState,
    ) -> Result<()> {
        let mut stack: Vec<GraphicsState> = Vec::new();
        let mut skipped_saves = 0usize;
        let mut path = Path::with_tolerance(0.25);
        let mut pending_clip: Option<FillRule> = None;
        let mut text_clip: Option<Path> = None;
        let mut text_group = None;
        let inherited_visibility = self.visible;
        let mut marked_visibility = Vec::new();

        for (operation_index, op) in content.iter().enumerate() {
            if !self.spend(1) {
                break;
            }
            if skipped_saves != 0 {
                match op.operator {
                    "q" => skipped_saves += 1,
                    "Q" => skipped_saves -= 1,
                    _ => {}
                }
                continue;
            }
            // Inside an uncoloured (d1) Type3 glyph the colour belongs to the
            // text being shown, so the glyph's own colour operators are
            // ignored (ISO 32000-1, 9.6.5) — it is a stencil, not a picture.
            if state.color_locked && is_color_operator(op.operator) {
                continue;
            }
            let n = |i: usize| op.operands.number(i).unwrap_or(0.0);
            match op.operator {
                // -- Type3 glyph metrics ---------------------------------------
                // `d0` declares a coloured glyph, `d1` an uncoloured one. Both
                // also give the advance, which /Widths already supplied.
                "d1" => state.color_locked = true,
                "d0" => {}

                // -- graphics state ------------------------------------------
                "q" => {
                    if stack.len() >= 128 {
                        self.warn("content skipped: graphics-state nesting exceeds renderer limit");
                        skipped_saves = 1;
                    } else {
                        stack.push(state.clone());
                    }
                }
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
                "J" => state.line_cap = n(0) as i64,
                "j" => state.line_join = n(0) as i64,
                "d" => {
                    let values: Vec<PdfObject> =
                        op.operands.iter().map(Operand::to_object).collect();
                    self.set_dash(&values, state)
                }
                "gs" => self.apply_ext_gstate(&op, resources, state),

                // -- path construction ---------------------------------------
                "m" => path.move_to(n(0), n(1)),
                "l" => path.line_to(n(0), n(1)),
                "c" => path.curve_to(n(0), n(1), n(2), n(3), n(4), n(5)),
                "v" => {
                    let current = path
                        .current_point()
                        .unwrap_or(crate::geom::Point::new(n(0), n(1)));
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
                    self.paint(op.operator, &mut path, resources, state, &mut pending_clip);
                    path.clear();
                }

                // -- colour ----------------------------------------------------
                "g" => {
                    state.fill_pattern_space = false;
                    state.fill_pattern = None;
                    state.fill_tint = None;
                    state.fill = Rgb::gray(n(0) as f32);
                    state.fill_components = 1;
                }
                "G" => {
                    state.stroke_pattern_space = false;
                    state.stroke_pattern = None;
                    state.stroke_tint = None;
                    state.stroke = Rgb::gray(n(0) as f32);
                    state.stroke_components = 1;
                }
                "rg" => {
                    state.fill_pattern_space = false;
                    state.fill_pattern = None;
                    state.fill_tint = None;
                    state.fill = Rgb::new(n(0) as f32, n(1) as f32, n(2) as f32);
                    state.fill_components = 3;
                }
                "RG" => {
                    state.stroke_pattern_space = false;
                    state.stroke_pattern = None;
                    state.stroke_tint = None;
                    state.stroke = Rgb::new(n(0) as f32, n(1) as f32, n(2) as f32);
                    state.stroke_components = 3;
                }
                "k" => {
                    state.fill_pattern_space = false;
                    state.fill_pattern = None;
                    state.fill_tint = None;
                    state.fill = Rgb::from_cmyk(n(0) as f32, n(1) as f32, n(2) as f32, n(3) as f32);
                    state.fill_components = 4;
                }
                "K" => {
                    state.stroke_pattern_space = false;
                    state.stroke_pattern = None;
                    state.stroke_tint = None;
                    state.stroke =
                        Rgb::from_cmyk(n(0) as f32, n(1) as f32, n(2) as f32, n(3) as f32);
                    state.stroke_components = 4;
                }
                "cs" | "CS" => {
                    let components = self.space_components(&op, resources);
                    let pattern = self.is_pattern_space(&op, resources);
                    let tint = self.space_tint_transform(&op, resources);
                    // Setting a space resets the colour to its initial value,
                    // which is black in every device and CIE space.
                    let black = Rgb::BLACK;
                    if op.operator == "cs" {
                        state.fill_pattern_space = pattern;
                        state.fill_pattern = None;
                        state.fill_components = components;
                        state.fill_tint = tint;
                        state.fill = black;
                    } else {
                        state.stroke_pattern_space = pattern;
                        state.stroke_pattern = None;
                        state.stroke_components = components;
                        state.stroke_tint = tint;
                        state.stroke = black;
                    }
                }
                "sc" | "scn" | "SC" | "SCN" => {
                    let is_fill = op.operator.starts_with('s');
                    let pattern = op
                        .operands
                        .last()
                        .and_then(Operand::as_name)
                        .map(str::to_owned);
                    if is_fill {
                        state.fill_pattern = pattern;
                    } else {
                        state.stroke_pattern = pattern;
                    }
                    let values: Vec<f64> = op.operands.iter().filter_map(Operand::number).collect();
                    let tint = if is_fill {
                        state.fill_tint.as_ref()
                    } else {
                        state.stroke_tint.as_ref()
                    };
                    // A Separation/DeviceN space carries ink amounts, which
                    // only its tint transform can turn into a colour.
                    let color = match tint {
                        Some(tint) => tint.to_rgb(&values),
                        None => color_from(&values),
                    };
                    if let Some(color) = color {
                        if is_fill {
                            state.fill = color;
                        } else {
                            state.stroke = color;
                        }
                    }
                }

                // -- text ------------------------------------------------------
                "BT" => {
                    if let Some(group) = text_group.take() {
                        self.finish_text_group(group, state);
                        self.warn("nested text object closed implicitly");
                    }
                    text_clip = None;
                    state.text_matrix = Matrix::IDENTITY;
                    state.line_matrix = Matrix::IDENTITY;
                    // Opaque Normal text needs no offscreen buffer. A gs in
                    // the object may make it transparent after its first
                    // glyph, so retain the original backdrop in that case.
                    // AutoCAD repeats an opaque gs in every text object, and
                    // a page-sized buffer for each made a plan take seconds.
                    let turns_transparent = (operation_index + 1..content.len())
                        .filter_map(|index| content.get(index))
                        .take_while(|operation| !matches!(operation.operator, "BT" | "ET"))
                        .any(|operation| {
                            operation.operator == "gs"
                                && self.ext_gstate_adds_transparency(&operation, resources)
                        });
                    if state.text_knockout && (self.text_needs_group(state) || turns_transparent) {
                        text_group = self.begin_text_group(state);
                    }
                }
                "ET" => {
                    if let Some(group) = text_group.take() {
                        self.finish_text_group(group, state);
                    }
                    if let Some(path) = text_clip.take() {
                        self.clip_path(&path, FillRule::NonZero, state);
                    }
                }
                "Tf" => {
                    state.font_size = n(1);
                    if let Some(name) = op.operands.first().and_then(Operand::as_name) {
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
                "Tr" => {
                    state.render_mode = op.operands.first().and_then(Operand::as_i64).unwrap_or(0)
                }
                "Tj" => {
                    if let Some(bytes) = string_operand(&op, 0) {
                        self.show_text(
                            bytes,
                            resources,
                            state,
                            &mut text_clip,
                            text_group.is_some(),
                        );
                    }
                }
                "'" => {
                    next_line(state);
                    if let Some(bytes) = string_operand(&op, 0) {
                        self.show_text(
                            bytes,
                            resources,
                            state,
                            &mut text_clip,
                            text_group.is_some(),
                        );
                    }
                }
                "\"" => {
                    state.word_spacing = n(0);
                    state.char_spacing = n(1);
                    next_line(state);
                    if let Some(bytes) = string_operand(&op, 2) {
                        self.show_text(
                            bytes,
                            resources,
                            state,
                            &mut text_clip,
                            text_group.is_some(),
                        );
                    }
                }
                "TJ" => {
                    if let Some(PdfObject::Array(items)) =
                        op.operands.first().and_then(Operand::object)
                    {
                        for item in items {
                            match item {
                                PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => {
                                    self.show_text(
                                        bytes,
                                        resources,
                                        state,
                                        &mut text_clip,
                                        text_group.is_some(),
                                    )
                                }
                                other => {
                                    if let Some(adjust) = as_number(other) {
                                        // Kerning is expressed in 1/1000 em,
                                        // subtracted from the advance — which
                                        // runs down the page in vertical
                                        // writing, where Th does not apply.
                                        let shift = -adjust / 1000.0 * state.font_size;
                                        let vertical =
                                            state.font.as_ref().is_some_and(|f| f.text.vertical);
                                        let step = if vertical {
                                            Matrix::translate(0.0, shift)
                                        } else {
                                            Matrix::translate(shift * state.horizontal_scale, 0.0)
                                        };
                                        state.text_matrix = step.then(&state.text_matrix);
                                    }
                                }
                            }
                        }
                    }
                }

                // -- XObjects ---------------------------------------------------
                "Do" => {
                    if let Some(name) = op.operands.first().and_then(Operand::as_name) {
                        self.draw_xobject(name, resources, state);
                    }
                }

                "sh" if self.visible => {
                    if let Some(name) = op.operands.first().and_then(Operand::as_name) {
                        if let Some(shading) = self.resource_object(resources, "Shading", name) {
                            self.draw_shading(&shading, resources, state, false);
                        } else {
                            self.warn("shading skipped: missing resource");
                        }
                    }
                }
                "BI" => {
                    if let Some(PdfObject::Stream(stream)) =
                        op.operands.first().and_then(Operand::object)
                    {
                        let mut stream = stream.clone();
                        self.resolve_inline_color_space(&mut stream, resources);
                        self.draw_image(&stream, state, None);
                    } else {
                        self.warn("inline image skipped: malformed or truncated image data");
                    }
                }
                "sh" => {}
                "BMC" | "BDC" => {
                    marked_visibility.push(self.visible);
                    if op.operator == "BDC"
                        && op.operands.first().and_then(Operand::as_name) == Some("OC")
                    {
                        let property = op.operands.get(1).and_then(|value| {
                            if let Some(name) = value.as_name() {
                                resources
                                    .get("Properties")
                                    .and_then(|v| self.doc.resolve_dict(v))
                                    .and_then(|d| d.get(name))
                                    .cloned()
                            } else {
                                Some(value.to_object())
                            }
                        });
                        if let Some(property) = property {
                            self.visible = self.visible && self.optional_visible(&property, 0);
                        } else {
                            self.warn("optional content has no readable layer property; visibility may differ");
                        }
                    }
                }
                "EMC" => self.visible = marked_visibility.pop().unwrap_or(inherited_visibility),
                "MP" | "DP" | "BX" | "EX" => {}
                "M" => state.miter_limit = n(0).max(1.0),
                "ri" | "i" => {} // device-space rendering intent and flatness hints
                other => self.warn(format!(
                    "unsupported content operator {other}; appearance may differ"
                )),
            }
        }
        if let Some(group) = text_group.take() {
            self.finish_text_group(group, state);
        }
        self.visible = inherited_visibility;
        if content.is_truncated() {
            // The stream went on past what may be parsed of it.
            self.warn(limits::TOO_COMPLEX);
        }
        Ok(())
    }

    fn paint(
        &mut self,
        operator: &str,
        path: &mut Path,
        resources: &Dictionary,
        state: &mut GraphicsState,
        pending_clip: &mut Option<FillRule>,
    ) {
        if operator.starts_with('b') || operator == "s" {
            path.close();
        }
        let fills = matches!(operator, "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
        let strokes = matches!(operator, "S" | "s" | "B" | "B*" | "b" | "b*");
        let reach = if strokes { stroke_reach(state) } else { 0.0 };
        // Judged from the corners of the path's own bounds, so a path off the
        // canvas is not even transformed.
        let shows = !self.off_canvas_box(path, &state.ctm, reach);
        if !shows && pending_clip.is_none() {
            return;
        }
        let device = path.transform(&state.ctm);

        let rule = if operator.ends_with('*') {
            FillRule::EvenOdd
        } else {
            FillRule::NonZero
        };

        if fills && shows && self.visible && !device.is_empty() && !state.fill_marks_nothing() {
            // A filled rectangle on its own — a table border, an underline, a
            // cell background — lands on whole pixels, as PDFium and
            // Acrobat draw it.
            // Anti-aliased at a fractional position, a one-pixel rule
            // smears into two grey rows; at phone resolution every border in
            // a form looked washed out next to the same page in Drive.
            // Not inside a Type3 glyph, though: a font drawn from tiny
            // squares would bloat into blobs.
            let snapped = if strokes || self.type3_depth > 0 {
                None
            } else {
                snap_rect_fill(&device)
            };
            self.paint_color(
                snapped.as_ref().unwrap_or(&device),
                rule,
                true,
                resources,
                state,
            );
        }
        if strokes && shows && self.visible && !device.is_empty() && !state.stroke_marks_nothing() {
            self.paint_stroke(path, resources, state, true);
        }

        // `W` names the clip, but it only takes effect after the painting
        // operator that follows it — which is this one.
        if let Some(clip_rule) = pending_clip.take() {
            // A rectangular clip widens to whole pixels, so what it frames
            // keeps sharp edges instead of fading out over a partial pixel.
            let snapped = if self.type3_depth > 0 {
                None
            } else {
                snap_rect_clip(&device)
            };
            self.clip_path(snapped.as_ref().unwrap_or(&device), clip_rule, state);
        }
    }

    fn apply_ext_gstate(
        &mut self,
        op: &ContentOp,
        resources: &Dictionary,
        state: &mut GraphicsState,
    ) {
        let Some(name) = op.operands.first().and_then(Operand::as_name) else {
            return;
        };
        let Some(dict) = self.resource(resources, "ExtGState", name) else {
            return;
        };
        self.apply_ext_gstate_dict(&dict, resources, state);
    }

    fn apply_ext_gstate_dict(
        &mut self,
        dict: &Dictionary,
        resources: &Dictionary,
        state: &mut GraphicsState,
    ) {
        let doc = self.doc;
        let numeric = |key: &str| {
            dict.get(key)
                .map(|v| doc.resolve_value(v))
                .as_ref()
                .and_then(as_number)
        };
        if let Some(value) = numeric("ca") {
            state.fill_alpha = value.clamp(0.0, 1.0) as f32;
        }
        if let Some(value) = numeric("CA") {
            state.stroke_alpha = value.clamp(0.0, 1.0) as f32;
        }
        if let Some(mode) = dict.get("BM").map(|v| self.doc.resolve_value(v)) {
            let selected = self.blend_mode(mode);
            state.blend_mode = selected.unwrap_or(BlendMode::Normal);
            if selected.is_none() {
                self.warn("unknown blend mode; Normal is used");
            }
        }
        if let Some(value) = dict.get("SMask").map(|v| self.doc.resolve_value(v)) {
            if value.as_name() == Some("None") {
                state.soft_mask = None;
            } else {
                state.soft_mask = self.render_soft_mask(&value, resources, state);
            }
        }
        if let Some(PdfObject::Bool(value)) = dict.get("AIS").map(|v| self.doc.resolve_value(v)) {
            state.alpha_is_shape = value;
        }
        if let Some(PdfObject::Bool(value)) = dict.get("TK").map(|v| self.doc.resolve_value(v)) {
            state.text_knockout = value;
        }
        if let Some(value) = numeric("ML") {
            state.miter_limit = value.max(1.0);
        }
        if let Some(value) = numeric("LW") {
            state.line_width = value;
        }
        if let Some(value) = numeric("LC").map(|v| v as i64) {
            state.line_cap = value;
        }
        if let Some(value) = numeric("LJ").map(|v| v as i64) {
            state.line_join = value;
        }
        if let Some(PdfObject::Array(values)) = dict.get("D").map(|v| self.doc.resolve_value(v)) {
            self.set_dash(&values, state);
        }
        if let Some(PdfObject::Array(values)) = dict.get("Font").map(|v| doc.resolve_value(v)) {
            if let (Some(font), Some(size)) = (
                values.first().and_then(|v| doc.resolve_dict(v)),
                values
                    .get(1)
                    .map(|v| doc.resolve_value(v))
                    .as_ref()
                    .and_then(as_number),
            ) {
                state.font = Some(Arc::new(RenderFont::load(doc, font)));
                state.font_size = size;
            }
        }
    }

    /// Whether painting `device` (a path in device space) can change no pixel:
    /// its bounds, grown by `reach` for a stroke, lie wholly off the canvas.
    /// A region of a zoomed-in page then pays for what is in it, not for
    /// every path on the page — a plan with 150,000 strokes took as long for
    /// a corner as for the whole sheet.
    pub(super) fn off_canvas(&self, device: &Path, reach: f64) -> bool {
        device
            .bounds()
            .is_some_and(|bounds| self.bounds_off_canvas(bounds, reach))
    }

    /// [`Renderer::off_canvas`] for device-space bounds.
    fn bounds_off_canvas(&self, (x0, y0, x1, y1): (f64, f64, f64, f64), reach: f64) -> bool {
        if ![x0, y0, x1, y1, reach].iter().all(|v| v.is_finite()) {
            return false;
        }
        // Anti-aliasing and snapping to whole pixels reach a little further.
        let reach = reach + 2.0;
        x1 + reach < 0.0
            || y1 + reach < 0.0
            || x0 - reach > self.canvas.width as f64
            || y0 - reach > self.canvas.height as f64
    }

    /// Whether a glyph advancing `advance` (text space) lies so far off the
    /// canvas that none of it can show, judged from a box four ems around it
    /// — further than any real glyph strays from its origin — before its
    /// outline is built. Most glyphs of a page are off a zoomed-in region,
    /// or off a band. Text that clips needs every outline, so it never is.
    fn glyph_off_canvas(&self, state: &GraphicsState, advance: f64) -> bool {
        if state.render_mode >= 4 {
            return false;
        }
        let em = 4.0
            * state
                .font_size
                .abs()
                .max((state.font_size * state.horizontal_scale).abs());
        if !(em.is_finite() && em > 0.0) {
            return false;
        }
        let matrix = state.text_matrix.then(&state.ctm);
        let (x0, x1) = (advance.min(0.0) - em, advance.max(0.0) + em);
        let (y0, y1) = (state.rise - em, state.rise + em);
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| matrix.apply(x, y));
        let (mut left, mut top) = (f64::INFINITY, f64::INFINITY);
        let (mut right, mut bottom) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in corners {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
        let reach = if matches!(state.render_mode, 1 | 2) {
            stroke_reach(state)
        } else {
            0.0
        };
        self.bounds_off_canvas((left, top, right, bottom), reach)
    }

    /// [`Renderer::off_canvas`] for `path` (user space) drawn through `ctm`,
    /// judged from the corners of its bounds without transforming it.
    fn off_canvas_box(&self, path: &Path, ctm: &Matrix, reach: f64) -> bool {
        let Some((x0, y0, x1, y1)) = path.bounds() else {
            return false;
        };
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| ctm.apply(x, y));
        let (mut left, mut top) = (f64::INFINITY, f64::INFINITY);
        let (mut right, mut bottom) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in corners {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
        self.bounds_off_canvas((left, top, right, bottom), reach)
    }

    /// The blend mode a `/BM` value selects: a name, or the first name in an
    /// array this renderer knows.
    fn blend_mode(&self, mode: PdfObject) -> Option<BlendMode> {
        match mode {
            PdfObject::Name(name) => BlendMode::from_name(&name),
            PdfObject::Array(items) => items.iter().find_map(|v| {
                self.doc
                    .resolve_value(v)
                    .as_name()
                    .and_then(BlendMode::from_name)
            }),
            _ => None,
        }
    }

    /// Whether the graphics state a `gs` names can make what follows
    /// transparent: an alpha below 1, a blend mode other than Normal, or a
    /// soft mask.
    fn ext_gstate_adds_transparency(&self, op: &ContentOp, resources: &Dictionary) -> bool {
        let Some(name) = op.operands.first().and_then(Operand::as_name) else {
            return false;
        };
        let Some(dict) = self.resource(resources, "ExtGState", name) else {
            return false;
        };
        let value = |key: &str| dict.get(key).map(|v| self.doc.resolve_value(v));
        let below_one = |key: &str| {
            value(key)
                .as_ref()
                .and_then(as_number)
                .is_some_and(|alpha| alpha < 1.0)
        };
        below_one("ca")
            || below_one("CA")
            || value("BM")
                .and_then(|mode| self.blend_mode(mode))
                .is_some_and(|mode| mode != BlendMode::Normal)
            || value("SMask").is_some_and(|mask| mask.as_name() != Some("None"))
    }

    fn space_components(&self, op: &ContentOp, resources: &Dictionary) -> usize {
        match op.operands.first().and_then(Operand::as_name) {
            Some(name) => match name {
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

    /// Set aside every cache keyed by resource name before running a nested
    /// content stream — a form, a tiling pattern, a Type3 glyph — that has its
    /// own `/Resources`. Inside it `/F1` or `/CS0` may name something else
    /// entirely, and a hit from the outer scope would draw with the wrong font
    /// or colour. Pair with [`Renderer::leave_resource_scope`].
    fn enter_resource_scope(&mut self) -> ResourceScope {
        ResourceScope {
            fonts: std::mem::take(&mut self.fonts),
            tint_transforms: std::mem::take(&mut self.tint_transforms),
        }
    }

    fn leave_resource_scope(&mut self, scope: ResourceScope) {
        self.fonts = scope.fonts;
        self.tint_transforms = scope.tint_transforms;
    }

    /// The tint transform for the space a `cs`/`CS` names, when it is a
    /// Separation or DeviceN space. Cached per resource name: the function
    /// can be a large sampled table and a page may set the space hundreds of
    /// times.
    fn space_tint_transform(
        &mut self,
        op: &ContentOp,
        resources: &Dictionary,
    ) -> Option<Rc<TintTransform>> {
        let Some(name) = op.operands.first().and_then(Operand::as_name) else {
            return None;
        };
        if matches!(
            name,
            "DeviceGray"
                | "G"
                | "CalGray"
                | "DeviceRGB"
                | "RGB"
                | "CalRGB"
                | "DeviceCMYK"
                | "CMYK"
                | "Pattern"
        ) {
            return None;
        }
        if let Some(cached) = self.tint_transforms.get(name) {
            return cached.clone();
        }
        let loaded = self
            .resource_object(resources, "ColorSpace", name)
            .and_then(|object| TintTransform::load(self.doc, &object))
            .map(Rc::new);
        self.tint_transforms.insert(name.to_owned(), loaded.clone());
        loaded
    }

    fn load_font(&mut self, name: &str, resources: &Dictionary) -> Option<Arc<RenderFont>> {
        // Cache per resource name — a page typically reuses a handful.
        if let Some(font) = self.fonts.get(name) {
            return Some(Arc::clone(font));
        }
        let dict = self.resource(resources, "Font", name)?;
        // A font object is loaded once for all the bands of a render.
        let font = match self.resource_id(resources, "Font", name) {
            Some(id) => self.shared.font(id, || RenderFont::load(self.doc, &dict)),
            None => Arc::new(RenderFont::load(self.doc, &dict)),
        };
        self.fonts.insert(name.to_owned(), Arc::clone(&font));
        Some(font)
    }

    fn show_text(
        &mut self,
        bytes: &[u8],
        resources: &Dictionary,
        state: &mut GraphicsState,
        text_clip: &mut Option<Path>,
        knockout_group_active: bool,
    ) {
        let Some(font) = state.font.clone() else {
            if state.render_mode != 3 {
                self.warn("some text could not be drawn: missing font resource");
            }
            return;
        };
        // Mode 3 is invisible; mode 7 contributes outlines to the clip at ET.
        let invisible = !self.visible || state.render_mode == 3;
        let units_per_em = font.units_per_em();

        if !invisible {
            if font.is_substituted() && !font.is_standard_substitute() {
                self.warn("some text uses a substitute font: the document did not embed its own");
            } else if !font.can_draw_glyphs() {
                self.warn("some text could not be drawn: no usable font outlines");
            }
        }

        // A code is at most four bytes, so only the bytes of the glyphs one
        // string may show are split into codes.
        let bytes = &bytes[..bytes.len().min(limits::MAX_STRING_GLYPHS * 4)];
        for code in font.text.codes(bytes) {
            if !self.spend(1) {
                break;
            }
            let width = font.advance_width(code) / 1000.0;

            if let Some(type3) = font.type3() {
                if !invisible {
                    if let Some(procedure) = type3.procedure(code, &font.text) {
                        // Glyph space -> text space -> user space -> device.
                        let glyph_ctm = type3
                            .font_matrix
                            .then(&Matrix::scale(
                                state.font_size * state.horizontal_scale,
                                state.font_size,
                            ))
                            .then(&Matrix::translate(0.0, state.rise))
                            .then(&state.text_matrix)
                            .then(&state.ctm);
                        let glyph_resources = type3.resources.as_ref().unwrap_or(resources);
                        self.draw_type3_glyph(
                            type3,
                            procedure,
                            glyph_resources,
                            glyph_ctm,
                            state,
                        );
                    }
                }
            } else if !invisible
                && font.can_draw_glyphs()
                && !self.glyph_off_canvas(state, width * state.font_size * state.horizontal_scale)
            {
                if let Some(outline) = font.outline(code) {
                    // Glyph space -> text space -> user space -> device.
                    let scale = Matrix::scale(
                        state.font_size * state.horizontal_scale / units_per_em,
                        state.font_size / units_per_em,
                    );
                    // In vertical writing the text position is the glyph's
                    // *vertical* origin, which sits (vx, vy) from the
                    // horizontal one its outline is drawn around (9.7.4.3).
                    let (origin_x, origin_y) = if font.text.vertical {
                        let (_, vx, vy) = font.text.vertical_metrics(code);
                        (
                            -vx / 1000.0 * state.font_size,
                            -vy / 1000.0 * state.font_size,
                        )
                    } else {
                        (0.0, 0.0)
                    };
                    let offset = Matrix::translate(origin_x, origin_y + state.rise);
                    let user = outline.transform(&scale.then(&offset).then(&state.text_matrix));
                    let device = user.transform(&state.ctm);
                    self.paint_glyph(&user, &device, resources, state, knockout_group_active);
                    if matches!(state.render_mode, 4..=7) {
                        text_clip
                            .get_or_insert_with(Path::new)
                            .subpaths
                            .extend(device.subpaths);
                    }
                }
            }

            let word = if font.text.is_space_code(code) {
                state.word_spacing
            } else {
                0.0
            };
            let step = if font.text.vertical {
                // ty = w1y × Tfs + Tc + Tw; w1y is negative, so text runs
                // down the page, and horizontal scaling does not apply.
                let (w1y, _, _) = font.text.vertical_metrics(code);
                Matrix::translate(
                    0.0,
                    w1y / 1000.0 * state.font_size + state.char_spacing + word,
                )
            } else {
                let advance =
                    (width * state.font_size + state.char_spacing + word) * state.horizontal_scale;
                Matrix::translate(advance, 0.0)
            };
            state.text_matrix = step.then(&state.text_matrix);
        }
    }

    /// Run one Type3 glyph procedure with its glyph space mapped onto the
    /// page. It is a self-contained content stream: its graphics-state
    /// changes do not leak into the text that follows.
    fn draw_type3_glyph(
        &mut self,
        type3: &crate::font::Type3Glyphs,
        procedure: &crate::font::Type3Procedure,
        resources: &Dictionary,
        glyph_ctm: Matrix,
        state: &GraphicsState,
    ) {
        if self.depth >= 12 {
            self.warn("Type3 glyph skipped: recursive content exceeds renderer limit");
            return;
        }
        if !self.in_time() {
            return;
        }
        // Parsed the first time the glyph is drawn, and usually kept with
        // the font.
        let Some(content) = type3.content(procedure, self.doc) else {
            self.warn("Type3 glyph skipped: its procedure cannot be decoded");
            return;
        };
        if !self.hold_content(&content) {
            return;
        }
        let mut inner = state.clone();
        inner.ctm = glyph_ctm;
        inner.pattern_ctm = glyph_ctm;
        let outer = self.enter_resource_scope();
        self.depth += 1;
        self.type3_depth += 1;
        if self.run_operations(&content, resources, &mut inner).is_err() {
            self.warn("Type3 glyph skipped: malformed procedure");
        }
        self.type3_depth -= 1;
        self.depth -= 1;
        self.leave_resource_scope(outer);
        self.release_content(&content);
        self.configure_canvas(state);
    }

    fn draw_xobject(&mut self, name: &str, resources: &Dictionary, state: &mut GraphicsState) {
        if !self.visible {
            return;
        }
        if self.depth > 12 {
            self.warn("form skipped: recursive content exceeds renderer limit");
            return;
        }
        let Some(object) = self.resource_object(resources, "XObject", name) else {
            return;
        };
        let PdfObject::Stream(stream) = object else {
            return;
        };
        if stream
            .dictionary
            .get("OC")
            .is_some_and(|value| !self.optional_visible(value, 0))
        {
            return;
        }
        let subtype = stream
            .dictionary
            .get("Subtype")
            .and_then(PdfObject::as_name)
            .unwrap_or("");

        match subtype {
            "Image" => {
                let id = self.resource_id(resources, "XObject", name);
                self.draw_image(&stream, state, id);
            }
            "Form" => {
                let id = self.resource_id(resources, "XObject", name);
                self.draw_form(&stream, resources, state, id);
            }
            _ => {}
        }
    }

    fn resource(&self, resources: &Dictionary, category: &str, name: &str) -> Option<Dictionary> {
        let object = self.resource_object(resources, category, name)?;
        self.doc.resolve_dict(&object).cloned()
    }

    /// The object a named resource refers to, when it is a reference.
    fn resource_id(&self, resources: &Dictionary, category: &str, name: &str) -> Option<ObjectId> {
        let group = self.doc.resolve_value(resources.get(category)?);
        match self.doc.resolve_dict(&group)?.get(name)? {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        }
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

/// `(left, top, right, bottom)` when `path` (device space) is one
/// axis-aligned rectangle, possibly with no width or no height.
fn axis_aligned_rect(path: &Path) -> Option<(f64, f64, f64, f64)> {
    const EPSILON: f64 = 1e-6;
    let mut subpaths = path.subpaths.iter().filter(|s| s.points.len() > 1);
    let subpath = subpaths.next()?;
    if subpaths.next().is_some() {
        return None;
    }
    let mut points = subpath.points.as_slice();
    if points.len() == 5
        && (points[4].x - points[0].x).abs() < EPSILON
        && (points[4].y - points[0].y).abs() < EPSILON
    {
        points = &points[..4];
    }
    if points.len() != 4 || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return None;
    }
    let left = points.iter().map(|p| p.x).fold(f64::MAX, f64::min);
    let right = points.iter().map(|p| p.x).fold(f64::MIN, f64::max);
    let top = points.iter().map(|p| p.y).fold(f64::MAX, f64::min);
    let bottom = points.iter().map(|p| p.y).fold(f64::MIN, f64::max);
    let on =
        |value: f64, a: f64, b: f64| (value - a).abs() < EPSILON || (value - b).abs() < EPSILON;
    // Every point is a corner, every edge runs along an axis, and the four
    // points visit the four corners (rather than doubling back along a line).
    let mut corners = [false; 4];
    for (i, p) in points.iter().enumerate() {
        if !on(p.x, left, right) || !on(p.y, top, bottom) {
            return None;
        }
        let q = points[(i + 1) % 4];
        if (p.x - q.x).abs() >= EPSILON && (p.y - q.y).abs() >= EPSILON {
            return None;
        }
        let column = usize::from((p.x - right).abs() < EPSILON && right - left >= EPSILON);
        let row = usize::from((p.y - bottom).abs() < EPSILON && bottom - top >= EPSILON);
        corners[row * 2 + column] = true;
    }
    let wide = right - left >= EPSILON;
    let tall = bottom - top >= EPSILON;
    if wide && tall && !corners.iter().all(|&c| c) {
        return None;
    }
    Some((left, top, right, bottom))
}

/// How far, in device pixels, a stroke can reach from its path: half its
/// width (never under a pixel wide), further at mitred corners and square
/// caps.
fn stroke_reach(state: &GraphicsState) -> f64 {
    let ctm = &state.ctm;
    let scale = ctm.a.hypot(ctm.b).max(ctm.c.hypot(ctm.d));
    let half = (state.line_width.abs() * scale).max(1.0) / 2.0;
    let corner = if state.line_join == 0 {
        state.miter_limit.max(std::f64::consts::SQRT_2)
    } else {
        std::f64::consts::SQRT_2
    };
    half * corner
}

/// A filled rectangle on whole pixels, as Acrobat draws it: its size rounded
/// to the nearest whole number of pixels (never none), centred where it was.
/// A sliver that would anti-alias to a faint grey line becomes one solid
/// pixel row instead. (PDFium rounds the size up, which drew a 1.45-pixel
/// rule two pixels thick; Acrobat's one pixel is truer to the document.)
fn snap_rect_fill(path: &Path) -> Option<Path> {
    let (left, top, right, bottom) = axis_aligned_rect(path)?;
    // Far off-canvas or absurdly large rectangles gain nothing from snapping.
    if [left, top, right, bottom].iter().any(|v| v.abs() > 1e7) {
        return None;
    }
    let snap = |low: f64, high: f64| {
        let size = (high - low).round().max(1.0);
        // Halves round up, not away from zero, so a band or region of the
        // page — shifted by whole pixels, often to negative coordinates —
        // snaps exactly as the whole page does.
        let start = ((low + high) / 2.0 - size / 2.0 + 0.5).floor();
        (start, start + size)
    };
    let (x0, x1) = snap(left, right);
    let (y0, y1) = snap(top, bottom);
    let mut snapped = Path::new();
    snapped.rect(x0, y0, x1 - x0, y1 - y0);
    Some(snapped)
}

/// A rectangular clip widened to the pixels it touches (PDFium does the same),
/// so the edges of whatever it frames stay sharp. Anything else clips as drawn.
fn snap_rect_clip(path: &Path) -> Option<Path> {
    let (left, top, right, bottom) = axis_aligned_rect(path)?;
    // A clip with no area shows nothing; widening it would show a pixel.
    if right - left < 1e-6 || bottom - top < 1e-6 {
        return None;
    }
    if [left, top, right, bottom].iter().any(|v| v.abs() > 1e7) {
        return None;
    }
    let mut snapped = Path::new();
    let (x0, y0) = (left.floor(), top.floor());
    snapped.rect(x0, y0, right.ceil() - x0, bottom.ceil() - y0);
    Some(snapped)
}

/// Operators that set a colour or colour space — everything an uncoloured
/// Type3 glyph must not do.
fn is_color_operator(operator: &str) -> bool {
    matches!(
        operator,
        "g" | "G" | "rg" | "RG" | "k" | "K" | "cs" | "CS" | "sc" | "SC" | "scn" | "SCN"
    )
}

/// A Separation or DeviceN space's tint transform, plus the alternate space
/// its output lands in.
///
/// These spaces measure *ink*, not light: in `/Separation /Black`, a tint of 1
/// is solid black and 0 is bare paper. Reading the tint as a grey level paints
/// exactly the inverse, which turns black line art white and makes it vanish
/// against the page. The transform is the only correct way across, and it is
/// what the alternate space expects.
struct TintTransform {
    /// `None` for `/Separation /None`, which marks nothing on any device.
    function: Option<ColorFunction>,
    /// Component count of the alternate space the function outputs into.
    alternate_components: usize,
}

impl TintTransform {
    /// Build one for a Separation or DeviceN space, or `None` for any other
    /// space (which needs no conversion).
    fn load(doc: &PdfDocument, object: &PdfObject) -> Option<TintTransform> {
        let PdfObject::Array(items) = doc.resolve_value(object) else {
            return None;
        };
        let kind = items.first().and_then(PdfObject::as_name)?;
        if kind != "Separation" && kind != "DeviceN" {
            return None;
        }
        let colorant = items.get(1).map(|o| doc.resolve_value(o));
        if kind == "Separation" && colorant.as_ref().and_then(PdfObject::as_name) == Some("None") {
            return Some(TintTransform {
                function: None,
                alternate_components: 1,
            });
        }
        let alternate = doc.resolve_value(items.get(2)?);
        Some(TintTransform {
            alternate_components: components_of_space(doc, &alternate),
            function: Some(ColorFunction::load(doc, items.get(3)?, 0, &mut 64)?),
        })
    }

    /// Convert tint values to RGB through the transform. `None` means paint
    /// nothing — either the space is `/None` or the function would not run.
    fn to_rgb(&self, tints: &[f64]) -> Option<Rgb> {
        let mut out = self.function.as_ref()?.evaluate_inputs(tints)?;
        // Trust the alternate space's component count over the function's
        // declared /Range, which some writers pad beyond the real output.
        out.truncate(self.alternate_components.max(1));
        color_from(&out)
    }
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
        3 => Some(Rgb::new(
            values[0] as f32,
            values[1] as f32,
            values[2] as f32,
        )),
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
/// A page's content streams decoded, joined and parsed: what every render of
/// the page starts from, kept with the document when it may be.
struct ParsedPage {
    content: Content,
    /// How many content streams were present, and how many would not decode.
    streams: usize,
    failed: usize,
    /// Why the first failure happened, for the error message.
    first_error: Option<String>,
}

impl ParsedPage {
    fn load(doc: &PdfDocument, page: &Dictionary) -> Self {
        // Parsing recovers from damage rather than failing; whatever it can
        // make of the content is drawn, with the annotations over it.
        let raw = page_content(doc, page);
        Self {
            content: Content::parse(&raw.data),
            streams: raw.streams,
            failed: raw.failed,
            first_error: raw.first_error,
        }
    }
}

struct PageContent {
    data: Vec<u8>,
    /// How many content streams were present, and how many would not decode.
    streams: usize,
    failed: usize,
    /// Why the first failure happened, for the error message.
    first_error: Option<String>,
}

fn page_content(doc: &PdfDocument, page: &Dictionary) -> PageContent {
    let mut content = PageContent {
        data: Vec::new(),
        streams: 0,
        failed: 0,
        first_error: None,
    };
    let Some(entry) = page.get("Contents") else {
        return content;
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
    content
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
    if values.len() != 4 || values.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some((
        values[0].min(values[2]),
        values[1].min(values[3]),
        values[0].max(values[2]),
        values[1].max(values[3]),
    ))
}

fn as_number(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(v) => Some(*v as f64),
        PdfObject::Real(v) => Some(*v),
        _ => None,
    }
}

fn string_operand<'a>(op: &ContentOp<'a>, index: usize) -> Option<&'a [u8]> {
    match op.operands.get(index)?.object()? {
        PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => Some(bytes),
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
    pub(super) fn doc_with_content(content: &str, media: [i64; 4]) -> PdfDocument {
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

    pub(super) fn pixel(page: &RenderedPage, x: u32, y: u32) -> (u8, u8, u8) {
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

#[cfg(test)]
#[path = "page_regressions.rs"]
mod regressions;

#[cfg(test)]
#[path = "page_transparency_tests.rs"]
mod transparency_tests;

#[cfg(test)]
#[path = "page_crisp_tests.rs"]
mod crisp_tests;
