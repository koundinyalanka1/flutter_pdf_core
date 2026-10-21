//! Form XObjects, transparency groups and graphics-state soft masks.
use super::paints::matrix;
use super::*;
use crate::canvas::{luminosity, BlendColorSpace, Mask};
use pdf_core::stream::PdfStream;

impl Renderer<'_> {
    /// Draw a form XObject; `id`, when it has one, keys its parsed content
    /// in the cache shared with the render's other bands and later renders.
    pub(super) fn draw_form(
        &mut self,
        stream: &PdfStream,
        resources: &Dictionary,
        state: &GraphicsState,
        id: Option<ObjectId>,
    ) {
        if self.depth >= 12 {
            self.warn("form skipped: recursive content exceeds renderer limit");
            return;
        }
        let mut inner = state.clone();
        inner.ctm = matrix(self.doc, &stream.dictionary).then(&state.ctm);
        inner.pattern_ctm = inner.ctm;
        let clip = array_rect(self.doc, &stream.dictionary, "BBox").map(|bbox| {
            let mut p = Path::new();
            p.rect(bbox.0, bbox.1, bbox.2 - bbox.0, bbox.3 - bbox.1);
            p.transform(&inner.ctm)
        });
        // A form wholly off the canvas draws nothing; it is not parsed, nor
        // its clip made, nor counted against the render's forms.
        if clip
            .as_ref()
            .is_some_and(|device| self.off_canvas(device, 0.0))
        {
            return;
        }
        if !self.enter_form() {
            return;
        }
        if let Some(device) = clip {
            self.clip_path(&device, FillRule::NonZero, &mut inner);
        }
        if self.render_stopped {
            return;
        }
        let group = stream
            .dictionary
            .get("Group")
            .and_then(|o| self.doc.resolve_dict(o))
            .cloned()
            .filter(|d| {
                d.get("S")
                    .map(|o| self.doc.resolve_value(o))
                    .as_ref()
                    .and_then(PdfObject::as_name)
                    == Some("Transparency")
            });
        let inner_resources = stream
            .dictionary
            .get("Resources")
            .and_then(|o| self.doc.resolve_dict(o))
            .cloned()
            .unwrap_or_else(|| resources.clone());
        let doc = self.doc;
        // Parsed into what this band may still hold, so that forms nested
        // in one another cannot each take the most any content may.
        let room = self.content_room();
        let parse = || Some(Content::parse_within(&doc.stream_data(stream).ok()?, room));
        let content = match id {
            Some(id) => self.shared.form(id, parse),
            None => parse().map(Arc::new),
        };
        let Some(content) = content else {
            self.warn("form or annotation appearance skipped: content cannot be decoded");
            return;
        };
        if !self.hold_parsed(&content, room) {
            // Cut short by this render, not by the form: a later render
            // parses it afresh.
            if let Some(id) = id {
                self.shared.forget_form(id);
            }
            return;
        }
        let mut layer = None;
        if let Some(group) = group {
            let isolated = boolean(self.doc, &group, "I") || group.contains_key("CS");
            let knockout = boolean(self.doc, &group, "K");
            let mut blend_space = self.canvas.blend_color_space;
            if let Some(space) = group.get("CS").map(|o| self.doc.resolve_value(o)) {
                blend_space = if space.as_name() == Some("DeviceGray") {
                    BlendColorSpace::DeviceGray
                } else {
                    BlendColorSpace::DeviceRgb
                };
                if !matches!(space.as_name(), Some("DeviceRGB" | "DeviceGray")) {
                    self.warn(
                        "transparency group colour space converted to RGB; colour may differ",
                    );
                }
            }
            let Some(bytes) = self.reserve_layer(if knockout { 16 } else { 12 }) else {
                self.release_content(&content);
                return;
            };
            let offscreen =
                Canvas::group_in_color_space(self.canvas, isolated, knockout, blend_space);
            let parent = std::mem::replace(self.canvas, offscreen);
            layer = Some((parent, isolated, bytes));
            inner.blend_mode = BlendMode::Normal;
            inner.fill_alpha = 1.0;
            inner.stroke_alpha = 1.0;
            inner.soft_mask = None;
        }
        let outer = self.enter_resource_scope();
        self.depth += 1;
        if self
            .run_operations(&content, &inner_resources, &mut inner)
            .is_err()
        {
            self.warn("form or annotation appearance skipped: malformed content stream");
        }
        self.depth -= 1;
        self.leave_resource_scope(outer);
        self.release_content(&content);
        if let Some((parent, isolated, bytes)) = layer {
            let rendered = std::mem::replace(self.canvas, parent);
            self.configure_canvas(state);
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
                    let out_alpha = rendered.pixels[offset + 3] as f32 / 255.0;
                    let backdrop = self.canvas.group_backdrop();
                    let backdrop_alpha = backdrop[offset + 3] as f32 / 255.0;
                    for (c, value) in color.iter_mut().enumerate() {
                        let result = rendered.pixels[offset + c] as f32 / 255.0;
                        // Remove the initial backdrop before compositing the group once.
                        *value = if isolated {
                            result
                        } else {
                            (out_alpha * result
                                - (1.0 - alpha) * backdrop_alpha * backdrop[offset + c] as f32
                                    / 255.0)
                                / alpha
                        }
                        .clamp(0.0, 1.0);
                    }
                }
                // The inherited clip was already applied inside the group.
                self.canvas
                    .blend_at(index, color, alpha * state.fill_alpha, shape);
            }
            self.temporary_bytes -= bytes;
        }
        self.configure_canvas(state);
    }

    pub(super) fn render_soft_mask(
        &mut self,
        object: &PdfObject,
        resources: &Dictionary,
        state: &GraphicsState,
    ) -> ClipMask {
        let Some(dict) = self.doc.resolve_dict(object).cloned() else {
            self.warn("invalid graphics-state soft mask ignored");
            return None;
        };
        let subtype = dict.get("S").map(|o| self.doc.resolve_value(o));
        let luminosity_mask = match subtype.as_ref().and_then(PdfObject::as_name) {
            Some("Alpha") => false,
            Some("Luminosity") => true,
            _ => {
                self.warn("invalid soft-mask subtype ignored");
                return None;
            }
        };
        let Some(PdfObject::Stream(form)) = dict.get("G").map(|o| self.doc.resolve_value(o)) else {
            self.warn("soft mask skipped: missing group stream");
            return None;
        };
        if self.depth >= 12 {
            self.warn("soft mask skipped: recursive content exceeds renderer limit");
            return None;
        }
        let transfer = match dict.get("TR").map(|o| self.doc.resolve_value(o)) {
            None => None,
            Some(PdfObject::Name(name)) if name == "Identity" => None,
            Some(value) => match paints::scalar_function_table(self.doc, &value) {
                Some(values) => Some(values),
                None => {
                    self.warn("invalid soft-mask transfer function ignored");
                    None
                }
            },
        };
        let bytes = self.reserve_layer(5)?;
        let mut offscreen = Canvas::transparent(self.canvas.width, self.canvas.height);
        if luminosity_mask {
            let background = dict
                .get("BC")
                .map(|o| self.doc.resolve_value(o))
                .and_then(|o| match o {
                    PdfObject::Array(values) => {
                        let numbers: Option<Vec<_>> = values
                            .iter()
                            .map(|v| as_number(&self.doc.resolve_value(v)))
                            .collect();
                        numbers.and_then(|v| color_from(&v))
                    }
                    _ => None,
                })
                .unwrap_or(Rgb::BLACK);
            offscreen.fill_background(background);
        }
        let parent = std::mem::replace(self.canvas, offscreen);
        let mut inner = state.clone();
        inner.blend_mode = BlendMode::Normal;
        inner.fill_alpha = 1.0;
        inner.stroke_alpha = 1.0;
        inner.soft_mask = None;
        inner.clip = None;
        inner.clip_bounds = None;
        self.draw_form(&form, resources, &inner, None);
        let rendered = std::mem::replace(self.canvas, parent);
        self.configure_canvas(state);
        if self.render_stopped {
            self.temporary_bytes -= bytes;
            return None;
        }
        let data = rendered
            .pixels
            .chunks_exact(4)
            .map(|pixel| {
                let value = if luminosity_mask {
                    luminosity([
                        pixel[0] as f32 / 255.0,
                        pixel[1] as f32 / 255.0,
                        pixel[2] as f32 / 255.0,
                    ])
                } else {
                    pixel[3] as f32 / 255.0
                };
                let value = if let Some(table) = &transfer {
                    let position = value.clamp(0.0, 1.0) as f64 * 255.0;
                    let lo = position.floor() as usize;
                    table[lo] + (table[(lo + 1).min(255)] - table[lo]) * position.fract()
                } else {
                    value as f64
                };
                (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
            })
            .collect();
        self.temporary_bytes -= bytes;
        Some(self.register_mask(Mask {
            width: rendered.width,
            height: rendered.height,
            data,
        }))
    }
}

fn boolean(doc: &PdfDocument, dict: &Dictionary, key: &str) -> bool {
    matches!(
        dict.get(key).map(|o| doc.resolve_value(o)),
        Some(PdfObject::Bool(true))
    )
}
