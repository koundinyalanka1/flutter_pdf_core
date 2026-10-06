//! A shared budget for offscreen buffers and masks held by saved graphics states.
use super::*;
use crate::canvas::Mask;

pub(super) const MAX_TEMPORARY_BYTES: usize = 128 * 1024 * 1024;

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
        // Continuing with the preceding clip would paint outside the requested
        // bounds. Stop subsequent painting instead; keep existing page pixels.
        self.warn("page rendering stopped: clipping mask memory limit exceeded");
        self.render_stopped = true;
        self.canvas.paint_suppressed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(content: &str, limit: usize) -> (Canvas, Vec<String>, usize) {
        let doc = PdfDocument::new_empty("1.7");
        let mut canvas = Canvas::new(10, 10);
        let (warnings, retained) = {
            let mut renderer = Renderer {
                doc: &doc,
                canvas: &mut canvas,
                fonts: HashMap::new(),
                depth: 0,
                pattern_pixels: 0.0,
                temporary_bytes: 0,
                mask_allocations: Vec::new(),
                temporary_limit: limit,
                render_stopped: false,
                warnings: Vec::new(),
                visible: true,
            };
            renderer
                .run(
                    content.as_bytes(),
                    &Dictionary::new(),
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
