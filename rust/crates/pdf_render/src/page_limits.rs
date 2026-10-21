//! What a render may spend: memory for offscreen buffers and masks held by
//! saved graphics states, work, time, and the memory its pictures decode
//! into. A page past any of them stops drawing, keeps what it drew, and
//! says so in its warnings.
use super::*;
use crate::canvas::Mask;
use crate::image::{ImageBudget, MAX_PAGE_IMAGE_BYTES};
use pdf_text::content_stream::MAX_CONTENT_BYTES;
use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

pub(super) const MAX_TEMPORARY_BYTES: usize = 128 * 1024 * 1024;

/// Work one band of a render may do: an operator run, its forms', tiling
/// patterns' and Type3 glyphs' included, or a glyph shown, is one unit.
/// Every band reads the whole page, so this bounds each. A plan of 725,000
/// operators is well within it; a page past it is not one a phone shows.
pub(super) const MAX_WORK: u64 = 50_000_000;

/// Form XObjects one band may draw, however they nest. The nesting limit
/// alone allowed a form that draws itself three times half a million
/// draws, each with a page-sized clip: more than a minute on a phone.
pub(super) const MAX_FORM_CALLS: usize = 50_000;

/// Glyphs one string may show. A whole page of small print is a few
/// thousand.
pub(super) const MAX_STRING_GLYPHS: usize = 1 << 20;

/// Parsed content one band may hold at once for the forms, tiling patterns
/// and Type3 glyphs it is drawing one inside another; the page's own content
/// does not count. One form may be as large as any content may be, which
/// whole pages drawn as a form need; forms nested twelve deep, each that
/// large, held 1.5 GB. Content drawn inside others is parsed into what is
/// left, and a band with too little left stops.
pub(super) const MAX_HELD_CONTENT_BYTES: usize = MAX_CONTENT_BYTES;

/// Units of work between looks at the clock and the cancel flag.
const CLOCK_INTERVAL: u64 = 64;

/// How the warning of every render that stopped part way begins.
pub(super) const STOPPED: &str = "page rendering stopped:";

/// The warning a render that ran out of work, time or parse memory gives.
pub(super) const TOO_COMPLEX: &str = "page rendering stopped: page too complex to draw in full";

/// The warning a cancelled render gives.
pub(super) const CANCELLED: &str = "page rendering stopped: cancelled";

/// What every band of one render draws within: when to give up, whether
/// the caller has, and the memory its pictures may decode into.
pub(super) struct RenderBudget {
    deadline: Option<Instant>,
    cancel: RenderCancel,
    pub(super) images: ImageBudget,
    /// Image objects this render has decoded within [`RenderBudget::images`].
    /// The caches keep only the newest of the largest pictures, so a page
    /// that draws several of them decodes each again in every band, or each
    /// time it is drawn; such a decode replaces a copy let go, and is not
    /// taken from the budget again.
    paid: Mutex<HashSet<ObjectId>>,
}

impl RenderBudget {
    pub(super) fn new(deadline: Option<Instant>, cancel: RenderCancel) -> Self {
        Self {
            deadline,
            cancel,
            images: ImageBudget::new(MAX_PAGE_IMAGE_BYTES),
            paid: Mutex::default(),
        }
    }

    /// Whether image `id` has been decoded within the budget already.
    pub(super) fn is_paid_for(&self, id: ObjectId) -> bool {
        self.paid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id)
    }

    /// Note that image `id`, masks and all, was decoded within the budget.
    pub(super) fn paid_for(&self, id: ObjectId) {
        self.paid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
    }
}

/// Tiling-pattern work a page may take, as tiles times canvas pixels: each
/// tile is clipped by a canvas-sized mask, and tiny steps in an untrusted
/// file would otherwise stall a phone.
pub(super) const MAX_PATTERN_WORK: f64 = 256_000_000.0;

impl Renderer<'_> {
    fn live_mask_bytes(&mut self) -> usize {
        self.mask_allocations
            .retain(|mask| mask.strong_count() != 0);
        self.mask_allocations
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .fold(0usize, |bytes, mask| bytes.saturating_add(mask.data.len()))
    }

    pub(super) fn reserve_layer(&mut self, bytes_per_pixel: usize) -> Option<usize> {
        if self.render_stopped {
            return None;
        }
        let bytes = self
            .canvas
            .width
            .checked_mul(self.canvas.height)?
            .checked_mul(bytes_per_pixel)?;
        let used = self.temporary_bytes.saturating_add(self.live_mask_bytes());
        if bytes > self.temporary_limit.saturating_sub(used) {
            self.warn("graphics detail exceeds renderer memory limit");
            return None;
        }
        self.temporary_bytes += bytes;
        Some(bytes)
    }

    pub(super) fn register_mask(&mut self, mask: Mask) -> Rc<Mask> {
        let mask = Rc::new(mask);
        self.mask_allocations.push(Rc::downgrade(&mask));
        mask
    }

    pub(super) fn stop_for_clip_limit(&mut self) {
        // A render already stopped makes no clip, and was told why.
        if self.render_stopped {
            return;
        }
        // Continuing with the preceding clip would paint outside the requested
        // bounds. Stop subsequent painting instead; keep existing page pixels.
        self.stop_drawing("page rendering stopped: clipping mask memory limit exceeded");
    }

    /// Stop painting, keep the pixels drawn so far, and say why.
    fn stop_drawing(&mut self, why: &str) {
        self.warn(why);
        self.render_stopped = true;
        self.canvas.paint_suppressed = true;
    }

    /// Count `units` of work against [`MAX_WORK`], looking at the clock and
    /// the cancel flag every [`CLOCK_INTERVAL`] of them. False, with drawing
    /// stopped, once the work, the time or the caller's patience has run
    /// out.
    pub(super) fn spend(&mut self, units: u64) -> bool {
        if self.render_stopped {
            return false;
        }
        let before = self.work;
        self.work = self.work.saturating_add(units);
        if self.work > MAX_WORK {
            self.stop_drawing(TOO_COMPLEX);
            return false;
        }
        if before / CLOCK_INTERVAL != self.work / CLOCK_INTERVAL {
            return self.in_time();
        }
        true
    }

    /// Whether drawing may go on: false, with drawing stopped, once the
    /// render's deadline has passed or its caller has cancelled it. Asked
    /// before anything that can take a while on its own: a form, a Type3
    /// glyph, a tile, an image or a shading.
    pub(super) fn in_time(&mut self) -> bool {
        if self.render_stopped {
            return false;
        }
        if self.budget.cancel.is_cancelled() {
            self.stop_drawing(CANCELLED);
            return false;
        }
        if self
            .budget
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.stop_drawing(TOO_COMPLEX);
            return false;
        }
        true
    }

    /// Count a form XObject about to be drawn against [`MAX_FORM_CALLS`];
    /// false, with drawing stopped, when it may not be.
    pub(super) fn enter_form(&mut self) -> bool {
        if self.render_stopped {
            return false;
        }
        if self.form_calls >= MAX_FORM_CALLS {
            self.stop_drawing(TOO_COMPLEX);
            return false;
        }
        self.form_calls += 1;
        self.in_time()
    }

    /// The memory content drawn inside what this band now holds may be
    /// parsed into: what [`MAX_HELD_CONTENT_BYTES`] leaves, and never more
    /// than any content may take.
    pub(super) fn content_room(&self) -> usize {
        MAX_HELD_CONTENT_BYTES
            .saturating_sub(self.held_content)
            .min(MAX_CONTENT_BYTES)
    }

    /// Hold `content`, parsed into `room`, while it is drawn, until
    /// [`Renderer::release_content`]. False, with drawing stopped, when the
    /// room ran out before the content did: then it was cut short by what
    /// this band already holds, not by its own size, and is not drawn.
    pub(super) fn hold_parsed(&mut self, content: &Content, room: usize) -> bool {
        if content.is_truncated() && room < MAX_CONTENT_BYTES {
            self.stop_drawing(TOO_COMPLEX);
            return false;
        }
        self.held_content = self.held_content.saturating_add(content.byte_size());
        true
    }

    /// Hold `content`, parsed earlier, while it is drawn, until
    /// [`Renderer::release_content`]. False, with drawing stopped, when
    /// this band has no room left for it.
    pub(super) fn hold_content(&mut self, content: &Content) -> bool {
        let bytes = content.byte_size();
        if bytes > self.content_room() {
            self.stop_drawing(TOO_COMPLEX);
            return false;
        }
        self.held_content += bytes;
        true
    }

    /// Let go of content held by [`Renderer::hold_parsed`] or
    /// [`Renderer::hold_content`].
    pub(super) fn release_content(&mut self, content: &Content) {
        self.held_content = self.held_content.saturating_sub(content.byte_size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(content: &str, limit: usize) -> (Canvas, Vec<String>, usize) {
        run_with(content, &Dictionary::new(), limit)
    }

    fn run_with(
        content: &str,
        resources: &Dictionary,
        limit: usize,
    ) -> (Canvas, Vec<String>, usize) {
        let doc = PdfDocument::new_empty("1.7");
        let mut canvas = Canvas::new(10, 10);
        let shared = super::super::bands::Shared::default();
        let recent = super::super::bands::Recent::default();
        let budget = RenderBudget::new(None, RenderCancel::default());
        let (warnings, retained) = {
            let mut renderer = Renderer {
                doc: &doc,
                canvas: &mut canvas,
                shared: &shared,
                recent: &recent,
                fonts: HashMap::new(),
                tint_transforms: HashMap::new(),
                depth: 0,
                pattern_pixels: 0.0,
                origin: (0.0, 0.0),
                region: (0.0, 0.0, 10.0, 10.0),
                temporary_bytes: 0,
                mask_allocations: Vec::new(),
                temporary_limit: limit,
                render_stopped: false,
                warnings: Vec::new(),
                visible: true,
                type3_depth: 0,
                budget: &budget,
                work: 0,
                form_calls: 0,
                held_content: 0,
            };
            renderer
                .run_operations(
                    &Content::parse(content.as_bytes()),
                    resources,
                    &mut GraphicsState::new(Matrix::IDENTITY),
                )
                .unwrap();
            let retained = renderer.live_mask_bytes();
            (renderer.warnings, retained)
        };
        (canvas, warnings, retained)
    }

    #[test]
    fn saved_clips_share_a_budget_and_failure_does_not_paint_unclipped() {
        let content = format!("{}1 0 0 rg 0 0 10 10 re f", "q 0 0 5 5 re W n ".repeat(20));
        let (canvas, warnings, _) = run(&content, 350);
        assert!(warnings
            .iter()
            .any(|w| w.contains("clipping mask memory limit")));
        assert!(canvas.pixels.iter().all(|&v| v == 255));
    }

    #[test]
    fn restored_states_release_clip_budget_for_later_objects() {
        let content = format!(
            "{}1 0 0 rg 0 0 10 10 re f",
            "q 0 0 5 5 re W n Q ".repeat(200)
        );
        let (canvas, warnings, retained) = run(&content, 250);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(retained, 0);
        assert!(canvas.pixels.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
    }

    #[test]
    fn an_opaque_graphics_state_in_text_needs_no_offscreen_buffer() {
        // AutoCAD sets an opaque state inside every text object. Only a
        // state that adds transparency needs the page-sized knockout buffer.
        let resources = match pdf_core::parser::Parser::new(
            b"<< /ExtGState << /Opaque << /ca 1 /CA 1 /BM /Normal >> /Half << /ca 0.5 >> >> >>",
        )
        .parse_object()
        .unwrap()
        {
            PdfObject::Dictionary(dict) => dict,
            _ => unreachable!(),
        };
        let (_, warnings, _) = run_with("BT /Opaque gs ET", &resources, 10);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (_, warnings, _) = run_with("BT /Half gs ET", &resources, 10);
        assert!(
            warnings.iter().any(|w| w.contains("memory limit")),
            "{warnings:?}"
        );
    }

    #[test]
    fn clip_intersection_counts_both_scratch_and_saved_masks() {
        let (canvas, warnings, _) = run(
            "0 0 5 5 re W n q 0 0 4 4 re W n 1 0 0 rg 0 0 10 10 re f",
            250,
        );
        assert!(warnings
            .iter()
            .any(|w| w.contains("clipping mask memory limit")));
        assert!(canvas.pixels.iter().all(|&v| v == 255));
    }
}
