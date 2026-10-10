//! Text objects are implicit non-isolated knockout groups when TK is true.
//! ISO 32000-1 9.3.8 keeps graphics-state changes within BT/ET; transparency
//! applies to each glyph, with Normal/alpha 1 used for the completed group.
use super::*;

pub(super) struct TextGroup {
    parent: Canvas,
    bytes: usize,
}

impl Renderer<'_> {
    pub(super) fn text_needs_group(&self, state: &GraphicsState) -> bool {
        state.fill_alpha != 1.0
            || state.stroke_alpha != 1.0
            || state.soft_mask.is_some()
            || state.blend_mode != BlendMode::Normal
            || self.canvas.group_alpha.is_some()
    }

    pub(super) fn begin_text_group(&mut self, state: &GraphicsState) -> Option<TextGroup> {
        let bytes = self.reserve_layer(16)?;
        let offscreen = Canvas::group(self.canvas, false, true);
        let parent = std::mem::replace(self.canvas, offscreen);
        self.configure_canvas(state);
        Some(TextGroup { parent, bytes })
    }

    pub(super) fn finish_text_group(&mut self, group: TextGroup, state: &GraphicsState) {
        let rendered = std::mem::replace(self.canvas, group.parent);
        self.configure_canvas(state);
        // Alpha, masks, clipping and blend mode were applied to the glyphs.
        // Applying them again here changes both overlapping and lone glyphs.
        self.canvas.blend_mode = BlendMode::Normal;
        self.canvas.soft_mask = None;
        self.canvas.alpha_is_shape = false;
        let alphas = rendered.group_alpha.as_ref().unwrap();
        let shapes = rendered.group_shape.as_ref().unwrap();
        for (index, &alpha) in alphas.iter().enumerate() {
            let shape = shapes[index];
            if shape <= 0.0 {
                continue;
            }
            let offset = index * 4;
            let mut color = [0.0; 3];
            if alpha > 0.0 {
                let backdrop = self.canvas.group_backdrop();
                let out_alpha = rendered.pixels[offset + 3] as f32 / 255.0;
                let backdrop_alpha = backdrop[offset + 3] as f32 / 255.0;
                for (channel, value) in color.iter_mut().enumerate() {
                    *value = ((out_alpha * rendered.pixels[offset + channel] as f32 / 255.0
                        - (1.0 - alpha) * backdrop_alpha * backdrop[offset + channel] as f32
                            / 255.0)
                        / alpha)
                        .clamp(0.0, 1.0);
                }
            }
            self.canvas.blend_at(index, color, alpha, shape);
        }
        self.temporary_bytes -= group.bytes;
        self.configure_canvas(state);
    }

    pub(super) fn paint_glyph(
        &mut self,
        user: &Path,
        device: &Path,
        resources: &Dictionary,
        state: &GraphicsState,
        knockout_group_active: bool,
    ) {
        let fill = matches!(state.render_mode, 0 | 2 | 4 | 6) && !state.fill_marks_nothing();
        let stroke = matches!(state.render_mode, 1 | 2 | 5 | 6) && !state.stroke_marks_nothing();
        // Combined fill/stroke is itself one knockout object (11.7.4.4),
        // independently of TK. Inside the text's knockout group, painting
        // these two parts directly uses the same initial backdrop and is
        // equivalent to the nested non-isolated knockout group.
        let group = if fill && stroke && !knockout_group_active && self.text_needs_group(state) {
            let Some(group) = self.begin_text_group(state) else {
                return;
            };
            Some(group)
        } else {
            None
        };
        if fill {
            self.paint_color(device, FillRule::NonZero, true, resources, state);
        }
        if stroke {
            self.paint_stroke(user, resources, state);
        }
        if let Some(group) = group {
            self.finish_text_group(group, state);
        }
    }
}

#[cfg(test)]
#[path = "page_text_tests.rs"]
mod tests;
