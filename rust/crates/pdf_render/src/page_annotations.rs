//! Annotation appearance streams follow ISO 32000-1 12.5.5: transform the
//! appearance BBox by its Matrix, then fit that box to the annotation Rect.
//! Appearances are visual content only; no actions or JavaScript execute.
use super::paints::{bounds, matrix, numbers};
use super::*;

impl Renderer<'_> {
    fn annotation_state(&mut self, annotation: &Dictionary, ctm: Matrix) -> GraphicsState {
        let mut state = GraphicsState::new(ctm);
        let border = annotation.get("BS").and_then(|v| self.doc.resolve_dict(v));
        let legacy = numbers(self.doc, annotation.get("Border"));
        state.line_width = border
            .and_then(|d| d.get("W"))
            .and_then(as_number)
            .or_else(|| legacy.get(2).copied())
            .unwrap_or(1.0)
            .max(0.0);
        state.stroke = color_from(&numbers(self.doc, annotation.get("C"))).unwrap_or(Rgb::BLACK);
        state.stroke_alpha = annotation
            .get("CA")
            .and_then(as_number)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0) as f32;
        state.fill_alpha = state.stroke_alpha;
        if border.and_then(|d| d.get("S")).and_then(PdfObject::as_name) == Some("D") {
            let dash = border
                .and_then(|d| d.get("D"))
                .cloned()
                .unwrap_or_else(|| PdfObject::Array(vec![PdfObject::Integer(3)]));
            self.set_dash(&[dash, PdfObject::Integer(0)], &mut state);
        } else if let Some(PdfObject::Array(values)) =
            annotation.get("Border").map(|v| self.doc.resolve_value(v))
        {
            if let Some(dash) = values.get(3) {
                self.set_dash(&[dash.clone(), PdfObject::Integer(0)], &mut state);
            }
        }
        state
    }

    fn draw_geometric_annotation(
        &mut self,
        annotation: &Dictionary,
        subtype: &str,
        rect: (f64, f64, f64, f64),
        ctm: Matrix,
    ) {
        let mut state = self.annotation_state(annotation, ctm);
        let resources = Dictionary::new();
        let mut path = Path::new();
        let closed = matches!(subtype, "Square" | "Circle" | "Polygon");
        match subtype {
            "Square" | "Circle" => {
                let inset = numbers(self.doc, annotation.get("RD"));
                let inset = |i: usize| {
                    inset
                        .get(i)
                        .copied()
                        .unwrap_or(state.line_width / 2.0)
                        .max(0.0)
                };
                let r = (
                    rect.0 + inset(0),
                    rect.1 + inset(1),
                    rect.2 - inset(2),
                    rect.3 - inset(3),
                );
                if r.2 <= r.0 || r.3 <= r.1 {
                    return;
                }
                if subtype == "Square" {
                    path.rect(r.0, r.1, r.2 - r.0, r.3 - r.1);
                } else {
                    ellipse(&mut path, r);
                }
            }
            "Line" | "Polygon" | "PolyLine" => {
                let values = numbers(
                    self.doc,
                    annotation.get(if subtype == "Line" { "L" } else { "Vertices" }),
                );
                if values.len() < 4
                    || values.len() % 2 != 0
                    || values.iter().any(|v| !v.is_finite())
                {
                    self.warn(format!("{subtype} annotation skipped: invalid vertices"));
                    return;
                }
                path.move_to(values[0], values[1]);
                for xy in values[2..].chunks_exact(2) {
                    path.line_to(xy[0], xy[1]);
                }
                if closed {
                    path.close();
                }
                if !closed {
                    if let Some(PdfObject::Array(ends)) =
                        annotation.get("LE").map(|v| self.doc.resolve_value(v))
                    {
                        let last = values.len() - 2;
                        for (i, tip, neighbor) in [
                            (0, (values[0], values[1]), (values[2], values[3])),
                            (
                                1,
                                (values[last], values[last + 1]),
                                (values[last - 2], values[last - 1]),
                            ),
                        ] {
                            let ending = ends.get(i).and_then(PdfObject::as_name).unwrap_or("None");
                            let marker =
                                line_ending(ending, tip, neighbor, state.line_width.max(1.0) * 4.0);
                            if let Some((marker, fill)) = marker {
                                if fill {
                                    state.fill = state.stroke;
                                    self.paint_color(
                                        &marker.transform(&ctm),
                                        FillRule::NonZero,
                                        true,
                                        &resources,
                                        &state,
                                    );
                                }
                                self.paint_stroke(&marker, &resources, &state);
                            }
                        }
                    }
                }
            }
            "Ink" => {
                let Some(PdfObject::Array(strokes)) =
                    annotation.get("InkList").map(|v| self.doc.resolve_value(v))
                else {
                    self.warn("Ink annotation skipped: missing InkList");
                    return;
                };
                for stroke in strokes {
                    let values = numbers(self.doc, Some(&stroke));
                    if values.len() < 4
                        || values.len() % 2 != 0
                        || values.iter().any(|v| !v.is_finite())
                    {
                        continue;
                    }
                    path.move_to(values[0], values[1]);
                    for xy in values[2..].chunks_exact(2) {
                        path.line_to(xy[0], xy[1]);
                    }
                }
                state.line_cap = 1;
                state.line_join = 1;
            }
            _ => return,
        }
        if closed {
            if let Some(fill) = color_from(&numbers(self.doc, annotation.get("IC"))) {
                state.fill = fill;
                self.paint_color(
                    &path.transform(&ctm),
                    FillRule::NonZero,
                    true,
                    &resources,
                    &state,
                );
            }
        }
        if state.line_width > 0.0 {
            self.paint_stroke(&path, &resources, &state);
        }
        if annotation
            .get("BE")
            .and_then(|v| self.doc.resolve_dict(v))
            .and_then(|d| d.get("S"))
            .and_then(PdfObject::as_name)
            == Some("C")
        {
            self.warn("cloudy annotation border uses its underlying geometric outline");
        }
    }

    fn draw_free_text(&mut self, annotation: &Dictionary, rect: (f64, f64, f64, f64), ctm: Matrix) {
        let mut field = annotation.clone();
        field.insert("FT".into(), PdfObject::Name("Tx".into()));
        field.insert("Ff".into(), PdfObject::Integer(1 << 12));
        field.insert(
            "V".into(),
            annotation
                .get("Contents")
                .cloned()
                .unwrap_or(PdfObject::Null),
        );
        let mut mk = Dictionary::new();
        if let Some(c) = annotation.get("C") {
            mk.insert("BG".into(), c.clone());
        }
        if let Some(rotation) = annotation.get("Rotate") {
            mk.insert("R".into(), rotation.clone());
        }
        field.insert("MK".into(), PdfObject::Dictionary(mk));
        self.draw_widget_fallback(&field, rect, ctm);
        self.draw_link_border(annotation, rect, ctm);
        if annotation.contains_key("RC") {
            self.warn("FreeText rich text uses the plain Contents fallback");
        }
    }

    fn draw_annotation_icon(
        &mut self,
        annotation: &Dictionary,
        subtype: &str,
        rect: (f64, f64, f64, f64),
        ctm: Matrix,
    ) {
        self.warn(format!(
            "{subtype} annotation has no appearance; a generated icon is shown"
        ));
        let mut state = self.annotation_state(annotation, ctm);
        let resources = Dictionary::new();
        let (w, h) = (rect.2 - rect.0, rect.3 - rect.1);
        let mut shape = Path::new();
        if subtype == "Caret" {
            shape.move_to(rect.0, rect.1);
            shape.line_to((rect.0 + rect.2) * 0.5, rect.3);
            shape.line_to(rect.2, rect.1);
            shape.close();
        } else if subtype == "Sound" {
            ellipse(&mut shape, rect);
        } else {
            shape.rect(rect.0, rect.1, w, h);
        }
        state.fill =
            color_from(&numbers(self.doc, annotation.get("C"))).unwrap_or(Rgb::new(1.0, 0.9, 0.35));
        self.paint_color(
            &shape.transform(&ctm),
            FillRule::NonZero,
            true,
            &resources,
            &state,
        );
        state.stroke = Rgb::BLACK;
        self.paint_stroke(&shape, &resources, &state);
        let label = if subtype == "Stamp" {
            annotation
                .get("Name")
                .and_then(PdfObject::as_name)
                .unwrap_or("Draft")
        } else {
            match subtype {
                "Sound" => "S",
                "FileAttachment" => "@",
                "Caret" => "",
                _ => "N",
            }
        };
        if !label.is_empty() {
            let field = Dictionary::from([
                ("FT".into(), PdfObject::Name("Tx".into())),
                ("Q".into(), PdfObject::Integer(1)),
                (
                    "V".into(),
                    PdfObject::LiteralString(label.as_bytes().to_vec()),
                ),
                (
                    "Border".into(),
                    PdfObject::Array(vec![PdfObject::Integer(0); 3]),
                ),
            ]);
            self.draw_widget_fallback(&field, rect, ctm);
        }
    }

    pub(super) fn draw_annotations(
        &mut self,
        page: &Dictionary,
        resources: &Dictionary,
        ctm: Matrix,
    ) {
        let form = self
            .doc
            .catalog()
            .and_then(|c| c.get("AcroForm"))
            .and_then(|o| self.doc.resolve_dict(o));
        let default_resources = form
            .and_then(|d| d.get("DR"))
            .and_then(|o| self.doc.resolve_dict(o))
            .cloned();
        if form.is_some_and(|d| d.contains_key("XFA")) {
            self.warn("XFA form content is unsupported; available AcroForm appearances are shown");
        }
        let Some(PdfObject::Array(items)) = page.get("Annots").map(|o| self.doc.resolve_value(o))
        else {
            return;
        };
        for item in items {
            let Some(mut annotation) = self.doc.resolve_dict(&item).cloned() else {
                self.warn("annotation skipped: missing or malformed dictionary");
                continue;
            };
            if annotation
                .get("OC")
                .is_some_and(|value| !self.optional_visible(value, 0))
            {
                continue;
            }
            if annotation.get("Subtype").and_then(PdfObject::as_name) == Some("Widget") {
                // Field values and variable-text attributes live on ancestors
                // when a field has separate widgets (or several page widgets).
                let mut parent = annotation.get("Parent").cloned();
                let mut seen = std::collections::HashSet::new();
                for _ in 0..64 {
                    let Some(object) = parent else {
                        break;
                    };
                    if let Some(id) = object.as_ref() {
                        if !seen.insert(id) {
                            break;
                        }
                    }
                    let Some(field) = self.doc.resolve_dict(&object) else {
                        break;
                    };
                    for key in ["FT", "Ff", "V", "DA", "Q", "Opt", "MaxLen", "TI", "I"] {
                        if let Some(value) = field.get(key) {
                            annotation
                                .entry(key.into())
                                .or_insert_with(|| value.clone());
                        }
                    }
                    parent = field.get("Parent").cloned();
                }
            }
            let flags = annotation.get("F").and_then(PdfObject::as_i64).unwrap_or(0);
            if flags & (2 | 32) != 0 {
                continue;
            } // hidden or no-view
            let subtype = annotation
                .get("Subtype")
                .and_then(PdfObject::as_name)
                .unwrap_or("Unknown");
            // Invisible suppresses unknown annotations without a subtype
            // handler. Unlike Hidden, it does not hide standard PDF types.
            if flags & 1 != 0 && !standard_annotation_type(subtype) {
                continue;
            }
            // Popup annotations supply UI data, not page artwork. Links may be
            // deliberately borderless and have no appearance stream.
            if subtype == "Popup" {
                continue;
            }
            let Some(rect) = array_rect(self.doc, &annotation, "Rect") else {
                self.warn("annotation skipped: invalid or missing Rect");
                continue;
            };
            if rect.0 == rect.2 || rect.1 == rect.3 {
                continue;
            }
            if flags & 8 != 0 {
                self.warn("annotation NoZoom requires display zoom context; this page raster scales with the viewer");
            }
            let ctm = annotation_transform(ctm, rect, flags);
            self.configure_canvas(&GraphicsState::new(ctm));
            if let Some(stream) = self.annotation_appearance(&annotation) {
                let Some(bbox) = array_rect(self.doc, &stream.dictionary, "BBox") else {
                    self.warn("annotation appearance skipped: missing BBox");
                    continue;
                };
                let transformed = bounds(matrix(self.doc, &stream.dictionary), bbox);
                let (w, h) = (transformed.2 - transformed.0, transformed.3 - transformed.1);
                if !w.is_finite() || !h.is_finite() || w <= 1e-12 || h <= 1e-12 {
                    self.warn("annotation appearance skipped: degenerate bounding box");
                    continue;
                }
                let fit = Matrix::translate(-transformed.0, -transformed.1)
                    .then(&Matrix::scale((rect.2 - rect.0) / w, (rect.3 - rect.1) / h))
                    .then(&Matrix::translate(rect.0, rect.1));
                let mut state = GraphicsState::new(fit.then(&ctm));
                let mut clip = Path::new();
                clip.rect(rect.0, rect.1, rect.2 - rect.0, rect.3 - rect.1);
                self.clip_path(&clip.transform(&ctm), FillRule::NonZero, &mut state);
                let appearance_resources = if subtype == "Widget" {
                    default_resources.as_ref().unwrap_or(resources)
                } else {
                    resources
                };
                self.draw_form(&stream, appearance_resources, &state);
                continue;
            }
            let color = color_from(&numbers(self.doc, annotation.get("C")))
                .unwrap_or(Rgb::new(1.0, 1.0, 0.0));
            let alpha = annotation
                .get("CA")
                .and_then(as_number)
                .unwrap_or(1.0)
                .clamp(0.0, 1.0) as f32;
            match subtype {
                "Highlight" | "Underline" | "StrikeOut" | "Squiggly" => {
                    let quads = numbers(self.doc, annotation.get("QuadPoints"));
                    if quads.len() < 8 || quads.len() % 8 != 0 || quads.iter().any(|n| !n.is_finite()) {
                        self.warn("text-markup annotation skipped: invalid QuadPoints"); continue;
                    }
                    for quad in quads.chunks_exact(8) {
                        // Real-world markup uses both perimeter order and the
                        // Adobe top-left/top-right/bottom-left/bottom-right
                        // order. Sort around the centroid to accept either.
                        let mut points = [(quad[0],quad[1]),(quad[2],quad[3]),(quad[4],quad[5]),(quad[6],quad[7])];
                        let center = (points.iter().map(|p| p.0).sum::<f64>() / 4.0, points.iter().map(|p| p.1).sum::<f64>() / 4.0);
                        points.sort_by(|a,b| (a.1-center.1).atan2(a.0-center.0).total_cmp(&(b.1-center.1).atan2(b.0-center.0)));
                        let mut path = Path::new(); path.move_to(points[0].0,points[0].1);
                        for p in &points[1..] { path.line_to(p.0,p.1); } path.close();
                        if subtype == "Highlight" {
                            // Highlights multiply the backdrop so black text
                            // remains readable even without an explicit /AP.
                            let mask = self.canvas.rasterize_mask(&path.transform(&ctm), FillRule::NonZero);
                            for (i, coverage) in mask.data.iter().enumerate() {
                                let a = alpha * *coverage as f32 / 255.0;
                                if a == 0.0 { continue; }
                                for (channel, value) in [color.r,color.g,color.b].iter().enumerate() {
                                    let pixel = &mut self.canvas.pixels[i * 4 + channel];
                                    *pixel = (*pixel as f32 * (1.0 - a + a * value.clamp(0.0,1.0))).round() as u8;
                                }
                            }
                        } else {
                            let mut line = Path::new();
                            // QuadPoints' first edge follows the text direction,
                            // including text that is not horizontal on the page.
                            let baseline = markup_baseline(quad);
                            let (mut start, mut end, normal, height) = baseline;
                            if subtype == "StrikeOut" {
                                start.0 += normal.0 * height * 0.5;
                                start.1 += normal.1 * height * 0.5;
                                end.0 += normal.0 * height * 0.5;
                                end.1 += normal.1 * height * 0.5;
                            }
                            line.move_to(start.0, start.1);
                            if subtype == "Squiggly" {
                                let length = (end.0 - start.0).hypot(end.1 - start.1);
                                let amplitude = (height / 12.0).clamp(0.5, 2.0);
                                let steps = (length / (amplitude * 2.0)).ceil().clamp(1.0, 10_000.0) as usize;
                                for i in 1..=steps {
                                    let t = i as f64 / steps as f64;
                                    let bump = if i % 2 == 0 { 0.0 } else { amplitude };
                                    line.line_to(start.0 + (end.0 - start.0) * t + normal.0 * bump, start.1 + (end.1 - start.1) * t + normal.1 * bump);
                                }
                            } else { line.line_to(end.0, end.1); }
                            let outline = stroke_outline(&line, (height / 16.0).clamp(0.5, 2.0)).transform(&ctm);
                            self.canvas.fill_path(&outline, color, FillRule::NonZero, alpha, None);
                        }
                    }
                }
                "Link" => self.draw_link_border(&annotation, rect, ctm),
                "Widget" => self.draw_widget_fallback(&annotation, rect, ctm),
                "Square" | "Circle" | "Line" | "Polygon" | "PolyLine" | "Ink" => self.draw_geometric_annotation(&annotation, subtype, rect, ctm),
                "FreeText" => self.draw_free_text(&annotation, rect, ctm),
                "Text" | "Caret" | "Stamp" | "FileAttachment" | "Sound" => self.draw_annotation_icon(&annotation, subtype, rect, ctm),
                _ => self.warn(format!("{subtype} annotation has no usable appearance; open in a full PDF editor to regenerate it")),
            }
        }
    }

    fn annotation_appearance(
        &self,
        annotation: &Dictionary,
    ) -> Option<pdf_core::stream::PdfStream> {
        let appearances = self.doc.resolve_dict(annotation.get("AP")?)?;
        match self.doc.resolve_value(appearances.get("N")?) {
            PdfObject::Stream(stream) => Some(stream),
            PdfObject::Dictionary(states) => {
                // /AS names the appearance state; /V on the field is a useful
                // fallback for widgets with missing /AS. Never pick a random
                // "On" state, which could display an unchecked box as checked.
                let selected = annotation
                    .get("AS")
                    .or_else(|| annotation.get("V"))
                    .map(|v| self.doc.resolve_value(v));
                let entry = selected
                    .as_ref()
                    .and_then(PdfObject::as_name)
                    .and_then(|name| states.get(name))
                    .or_else(|| states.get("Off"))?;
                match self.doc.resolve_value(entry) {
                    PdfObject::Stream(s) => Some(s),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A missing appearance is common after filling a form without an editor
    /// that regenerates /AP. Render simple values readably and explicitly mark
    /// the approximation; never fabricate a missing signature appearance.
    fn draw_widget_fallback(
        &mut self,
        field: &Dictionary,
        rect: (f64, f64, f64, f64),
        ctm: Matrix,
    ) {
        let rotation = field
            .get("MK")
            .and_then(|v| self.doc.resolve_dict(v))
            .and_then(|d| d.get("R"))
            .and_then(PdfObject::as_i64)
            .unwrap_or(0);
        let (rect, ctm) = widget_transform(rect, ctm, rotation);
        let kind = field
            .get("FT")
            .and_then(PdfObject::as_name)
            .unwrap_or("Unknown");
        if !matches!(kind, "Tx" | "Ch" | "Btn") {
            self.warn(format!("{kind} widget has no usable appearance; open in a full PDF editor to regenerate it"));
            return;
        }
        self.warn("form widget appearance was missing; its value is drawn with approximate layout");
        let width = rect.2 - rect.0;
        let height = rect.3 - rect.1;
        let mut state = GraphicsState::new(ctm);
        let mut clip = Path::new();
        clip.rect(rect.0, rect.1, width, height);
        self.clip_path(&clip.transform(&ctm), FillRule::NonZero, &mut state);
        let appearance = field.get("MK").and_then(|o| self.doc.resolve_dict(o));
        if let Some(bg) = appearance.and_then(|d| color_from(&numbers(self.doc, d.get("BG")))) {
            self.canvas.fill_path(
                &clip.transform(&ctm),
                bg,
                FillRule::NonZero,
                1.0,
                state.clip.as_deref(),
            );
        }
        let border = field.get("BS").and_then(|o| self.doc.resolve_dict(o));
        let border_values = numbers(self.doc, field.get("Border"));
        let border_width = border
            .and_then(|d| d.get("W"))
            .and_then(as_number)
            .or_else(|| border_values.get(2).copied())
            .unwrap_or(1.0)
            .max(0.0);
        if let Some(color) = appearance.and_then(|d| color_from(&numbers(self.doc, d.get("BC")))) {
            let mut border_state = self.annotation_state(field, ctm);
            border_state.clip = state.clip.clone();
            border_state.stroke = color;
            self.draw_annotation_border(field, rect, &border_state);
        }
        let flags = field.get("Ff").and_then(PdfObject::as_i64).unwrap_or(0);
        let push_button = kind == "Btn" && flags & (1 << 16) != 0;
        if kind == "Btn" && !push_button {
            // /AS is per widget; it distinguishes individual radio buttons
            // even when their common parent holds the selected /V.
            let value = field
                .get("AS")
                .or_else(|| field.get("V"))
                .map(|v| self.doc.resolve_value(v));
            let checked = value
                .as_ref()
                .and_then(PdfObject::as_name)
                .is_some_and(|v| v != "Off");
            if flags & (1 << 15) != 0 && !field.contains_key("AS") {
                self.warn("radio widget has no appearance state; checked state cannot be inferred");
                return;
            }
            if checked {
                let mut tick = Path::new();
                let outline = if flags & (1 << 15) != 0 {
                    ellipse(
                        &mut tick,
                        (
                            rect.0 + width * 0.28,
                            rect.1 + height * 0.28,
                            rect.2 - width * 0.28,
                            rect.3 - height * 0.28,
                        ),
                    );
                    tick.transform(&ctm)
                } else {
                    tick.move_to(rect.0 + width * 0.2, rect.1 + height * 0.5);
                    tick.line_to(rect.0 + width * 0.43, rect.1 + height * 0.25);
                    tick.line_to(rect.0 + width * 0.8, rect.1 + height * 0.8);
                    stroke_outline(
                        &tick.transform(&ctm),
                        (width.min(height) * 0.1).max(1.0) * ctm.mean_scale(),
                    )
                };
                self.canvas.fill_path(
                    &outline,
                    Rgb::BLACK,
                    FillRule::NonZero,
                    1.0,
                    state.clip.as_deref(),
                );
            }
            return;
        }
        let value = field
            .get("V")
            .map(|v| self.doc.resolve_value(v))
            .unwrap_or(PdfObject::Null);
        let mut text = if push_button {
            appearance
                .and_then(|d| d.get("CA"))
                .map(field_text)
                .unwrap_or_default()
        } else {
            field_text(&value)
        };
        let mut text_rect = rect;
        if push_button {
            let position = appearance
                .and_then(|d| d.get("TP"))
                .and_then(PdfObject::as_i64)
                .unwrap_or(0);
            let mut icon_rect = rect;
            match position {
                1 => text.clear(),
                2 => {
                    text_rect.3 = rect.1 + height * 0.28;
                    icon_rect.1 = text_rect.3;
                }
                3 => {
                    text_rect.1 = rect.3 - height * 0.28;
                    icon_rect.3 = text_rect.1;
                }
                4 => {
                    text_rect.0 = rect.0 + width * 0.55;
                    icon_rect.2 = text_rect.0;
                }
                5 => {
                    text_rect.2 = rect.0 + width * 0.45;
                    icon_rect.0 = text_rect.2;
                }
                _ => {}
            }
            if position != 0 {
                if let Some(icon) = appearance
                    .and_then(|d| d.get("I"))
                    .map(|v| self.doc.resolve_value(v))
                {
                    if let PdfObject::Stream(icon) = icon {
                        let fit = appearance
                            .and_then(|d| d.get("IF"))
                            .and_then(|v| self.doc.resolve_dict(v))
                            .cloned()
                            .unwrap_or_default();
                        self.draw_button_icon(&icon, &fit, icon_rect, &state);
                    }
                }
            }
        }
        let mut choice_rows = None;
        if kind == "Ch" {
            // Choice /V contains export values. Prefer their visible labels.
            if let Some(PdfObject::Array(options)) =
                field.get("Opt").map(|v| self.doc.resolve_value(v))
            {
                let selected_values = match &value {
                    PdfObject::Array(values) => values.iter().map(field_text).collect::<Vec<_>>(),
                    _ => vec![text.clone()],
                };
                let indices = numbers(self.doc, field.get("I"));
                let mut rows = Vec::new();
                for (index, option) in options.into_iter().enumerate().take(4096) {
                    let option = self.doc.resolve_value(&option);
                    let (export, label) = match option {
                        PdfObject::Array(pair) if pair.len() == 2 => (
                            field_text(&self.doc.resolve_value(&pair[0])),
                            field_text(&self.doc.resolve_value(&pair[1])),
                        ),
                        value => {
                            let text = field_text(&value);
                            (text.clone(), text)
                        }
                    };
                    let selected = if indices.is_empty() {
                        selected_values.contains(&export)
                    } else {
                        indices.contains(&(index as f64))
                    };
                    if selected {
                        text = label.clone();
                    }
                    rows.push((label, selected));
                }
                if flags & (1 << 17) == 0 {
                    let top = field
                        .get("TI")
                        .and_then(PdfObject::as_i64)
                        .unwrap_or(0)
                        .max(0) as usize;
                    let rows = rows.into_iter().skip(top).collect::<Vec<_>>();
                    text = rows
                        .iter()
                        .map(|r| r.0.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    choice_rows = Some(rows);
                }
            }
        }
        if flags & (1 << 13) != 0 {
            text = "•".repeat(text.chars().count());
        } // password
        if text.is_empty() {
            return;
        }
        if text.chars().count() > 4096 {
            text = text.chars().take(4096).collect();
            self.warn("form widget fallback text truncated at renderer limit");
        }
        let form = self
            .doc
            .catalog()
            .and_then(|c| c.get("AcroForm"))
            .and_then(|o| self.doc.resolve_dict(o));
        let da = field.get("DA").or_else(|| form.and_then(|d| d.get("DA")));
        let da = da.map(|o| self.doc.resolve_value(o));
        let mut font_name = None;
        let mut size = 0.0;
        let mut color = Rgb::BLACK;
        if let Some(PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes)) = da {
            for op in parse_content(&bytes).unwrap_or_default() {
                match op.operator.as_str() {
                    "Tf" => {
                        font_name = op
                            .operands
                            .first()
                            .and_then(PdfObject::as_name)
                            .map(str::to_owned);
                        size = number(&op, 1);
                    }
                    "g" | "rg" | "k" => {
                        if let Some(c) = color_from(
                            &op.operands.iter().filter_map(as_number).collect::<Vec<_>>(),
                        ) {
                            color = c;
                        }
                    }
                    _ => {}
                }
            }
        }
        let resources = form
            .and_then(|d| d.get("DR"))
            .and_then(|o| self.doc.resolve_dict(o));
        let font_dict =
            resources.and_then(|r| font_name.as_ref().and_then(|n| self.resource(r, "Font", n)));
        let Some(mut font) = FieldFont::new(self.doc, font_dict.as_ref()) else {
            return;
        };
        self.configure_canvas(&state);
        let rect = text_rect;
        let width = rect.2 - rect.0;
        let height = rect.3 - rect.1;
        let padding = (border_width + 1.0).min(width / 4.0).min(height / 4.0);
        let available = (width - padding * 2.0).max(1.0);
        let multiline = choice_rows.is_some() || flags & (1 << 12) != 0;
        let comb = if kind == "Tx" && flags & (1 << 24) != 0 && !multiline && flags & (1 << 13) == 0
        {
            field
                .get("MaxLen")
                .and_then(PdfObject::as_i64)
                .filter(|v| (1..=4096).contains(v))
                .map(|v| v as usize)
        } else {
            None
        };
        if !size.is_finite() || size <= 0.0 {
            size = (height - padding * 2.0).min(12.0).max(1.0);
            if !multiline {
                let ems = text
                    .chars()
                    .map(|ch| font.glyph(ch).advance / 1000.0)
                    .sum::<f64>();
                if ems > 0.0 {
                    size = size.min(available / ems);
                }
            }
            if let Some(cells) = comb {
                let max_em = text
                    .chars()
                    .map(|ch| font.glyph(ch).advance / 1000.0)
                    .fold(0.0, f64::max);
                if max_em > 0.0 {
                    size = size.min(available / cells as f64 / max_em);
                }
            }
        }
        let mut lines = vec![String::new()];
        let mut line_width = 0.0;
        for ch in text.chars().filter(|c| *c != '\r') {
            let advance = font.glyph(ch).advance * size / 1000.0;
            if multiline
                && (ch == '\n'
                    || choice_rows.is_none()
                        && line_width + advance > available
                        && !lines.last().unwrap().is_empty())
            {
                lines.push(String::new());
                line_width = 0.0;
            }
            if ch == '\n' {
                if !multiline {
                    lines.last_mut().unwrap().push(' ');
                }
                continue;
            }
            lines.last_mut().unwrap().push(ch);
            line_width += advance;
        }
        let align = field
            .get("Q")
            .and_then(PdfObject::as_i64)
            .unwrap_or(if push_button { 1 } else { 0 });
        let mut y = if multiline {
            rect.3 - padding - size
        } else {
            rect.1 + (height - size) / 2.0 + size * 0.2
        };
        for (row, line) in lines.into_iter().enumerate() {
            if y + size < rect.1 {
                break;
            }
            let selected = choice_rows
                .as_ref()
                .and_then(|rows| rows.get(row))
                .is_some_and(|row| row.1);
            if selected {
                let mut background = Path::new();
                background.rect(rect.0 + padding, y - size * 0.2, available, size * 1.2);
                self.canvas.fill_path(
                    &background.transform(&ctm),
                    Rgb::new(0.15, 0.35, 0.7),
                    FillRule::NonZero,
                    1.0,
                    state.clip.as_deref(),
                );
            }
            let length = line
                .chars()
                .map(|ch| font.glyph(ch).advance * size / 1000.0)
                .sum::<f64>();
            let offset = match align {
                1 => (available - length) / 2.0,
                2 => available - length,
                _ => 0.0,
            }
            .max(0.0);
            let mut x = rect.0 + padding + offset;
            let count = line.chars().count();
            let cells_offset = comb
                .map(|cells| match align {
                    1 => (cells.saturating_sub(count)) / 2,
                    2 => cells.saturating_sub(count),
                    _ => 0,
                })
                .unwrap_or(0);
            for (index, ch) in line.chars().enumerate() {
                let glyph = font.glyph(ch);
                if let Some(cells) = comb {
                    if index >= cells {
                        break;
                    }
                    x = rect.0
                        + padding
                        + (index + cells_offset) as f64 * available / cells as f64
                        + (available / cells as f64 - glyph.advance * size / 1000.0) / 2.0;
                }
                if let Some(outline) = glyph.outline {
                    let transform =
                        Matrix::scale(size / glyph.units_per_em, size / glyph.units_per_em)
                            .then(&Matrix::translate(x, y))
                            .then(&ctm);
                    self.canvas.fill_path(
                        &outline.transform(&transform),
                        if selected {
                            Rgb::new(1.0, 1.0, 1.0)
                        } else {
                            color
                        },
                        FillRule::NonZero,
                        1.0,
                        state.clip.as_deref(),
                    );
                } else if !ch.is_whitespace() {
                    self.warn(
                        "some form field characters could not be drawn by the substitute font",
                    );
                }
                x += glyph.advance * size / 1000.0;
                if x >= rect.2 {
                    break;
                }
            }
            y -= size * 1.2;
        }
    }

    fn draw_button_icon(
        &mut self,
        icon: &pdf_core::stream::PdfStream,
        fit: &Dictionary,
        rect: (f64, f64, f64, f64),
        state: &GraphicsState,
    ) {
        let Some(bbox) = array_rect(self.doc, &icon.dictionary, "BBox") else {
            self.warn("button icon skipped: missing BBox");
            return;
        };
        let bbox = bounds(matrix(self.doc, &icon.dictionary), bbox);
        let (w, h) = (bbox.2 - bbox.0, bbox.3 - bbox.1);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (available_w, available_h) = (rect.2 - rect.0, rect.3 - rect.1);
        let (mut sx, mut sy) = (available_w / w, available_h / h);
        let scale = fit.get("SW").and_then(PdfObject::as_name).unwrap_or("A");
        if scale == "N"
            || scale == "B" && w <= available_w && h <= available_h
            || scale == "S" && (w >= available_w || h >= available_h)
        {
            sx = 1.0;
            sy = 1.0;
        } else if fit.get("S").and_then(PdfObject::as_name) != Some("A") {
            sx = sx.min(sy);
            sy = sx;
        }
        let align = numbers(self.doc, fit.get("A"));
        let x =
            rect.0 + (available_w - w * sx) * align.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let y =
            rect.1 + (available_h - h * sy) * align.get(1).copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let mut icon_state = state.clone();
        icon_state.ctm = Matrix::translate(-bbox.0, -bbox.1)
            .then(&Matrix::scale(sx, sy))
            .then(&Matrix::translate(x, y))
            .then(&state.ctm);
        let mut clip = Path::new();
        clip.rect(rect.0, rect.1, available_w, available_h);
        self.clip_path(
            &clip.transform(&state.ctm),
            FillRule::NonZero,
            &mut icon_state,
        );
        self.draw_form(icon, &Dictionary::new(), &icon_state);
    }

    fn draw_link_border(
        &mut self,
        annotation: &Dictionary,
        rect: (f64, f64, f64, f64),
        ctm: Matrix,
    ) {
        let color_values = numbers(self.doc, annotation.get("C"));
        if annotation.contains_key("C") && color_values.is_empty() {
            return;
        }
        let state = self.annotation_state(annotation, ctm);
        self.draw_annotation_border(annotation, rect, &state);
    }

    fn draw_annotation_border(
        &mut self,
        annotation: &Dictionary,
        rect: (f64, f64, f64, f64),
        state: &GraphicsState,
    ) {
        if state.line_width <= 0.0 {
            return;
        }
        let style = annotation
            .get("BS")
            .and_then(|v| self.doc.resolve_dict(v))
            .and_then(|d| d.get("S"))
            .and_then(PdfObject::as_name)
            .unwrap_or("S");
        let inset = state.line_width * 0.5;
        let mut path = Path::new();
        if style == "U" {
            path.move_to(rect.0, rect.1 + inset);
            path.line_to(rect.2, rect.1 + inset);
        } else {
            path.rect(
                rect.0 + inset,
                rect.1 + inset,
                (rect.2 - rect.0 - state.line_width).max(0.0),
                (rect.3 - rect.1 - state.line_width).max(0.0),
            );
        }
        self.paint_stroke(&path, &Dictionary::new(), state);
        if matches!(style, "B" | "I") {
            let d = state
                .line_width
                .min((rect.2 - rect.0) / 2.0)
                .min((rect.3 - rect.1) / 2.0);
            let mut top = Path::new();
            top.move_to(rect.0, rect.1);
            top.line_to(rect.0, rect.3);
            top.line_to(rect.2, rect.3);
            top.line_to(rect.2 - d, rect.3 - d);
            top.line_to(rect.0 + d, rect.3 - d);
            top.line_to(rect.0 + d, rect.1 + d);
            top.close();
            let mut bottom = Path::new();
            bottom.move_to(rect.0, rect.1);
            bottom.line_to(rect.2, rect.1);
            bottom.line_to(rect.2, rect.3);
            bottom.line_to(rect.2 - d, rect.3 - d);
            bottom.line_to(rect.2 - d, rect.1 + d);
            bottom.line_to(rect.0 + d, rect.1 + d);
            bottom.close();
            let (light, dark) = (Rgb::new(0.85, 0.85, 0.85), Rgb::new(0.3, 0.3, 0.3));
            for (path, color) in [
                (&top, if style == "B" { light } else { dark }),
                (&bottom, if style == "B" { dark } else { light }),
            ] {
                self.canvas.fill_path(
                    &path.transform(&state.ctm),
                    color,
                    FillRule::NonZero,
                    1.0,
                    state.clip.as_deref(),
                );
            }
        }
    }
}

#[derive(Clone)]
struct FieldGlyph {
    outline: Option<Path>,
    advance: f64,
    units_per_em: f64,
}

struct FieldFont {
    declared: Option<RenderFont>,
    fallback: crate::font::fallback::FallbackFont,
    glyphs: HashMap<char, FieldGlyph>,
}

impl FieldFont {
    fn new(doc: &PdfDocument, dictionary: Option<&Dictionary>) -> Option<Self> {
        let style = dictionary
            .and_then(|d| d.get("BaseFont"))
            .and_then(PdfObject::as_name)
            .unwrap_or("Helvetica");
        Some(Self {
            declared: dictionary.map(|dict| RenderFont::load(doc, dict)),
            fallback: crate::font::fallback::FallbackFont::for_style(
                crate::font::fallback::FallbackStyle::detect(style, 0),
            )?,
            glyphs: HashMap::new(),
        })
    }

    fn glyph(&mut self, ch: char) -> FieldGlyph {
        if let Some(glyph) = self.glyphs.get(&ch) {
            return glyph.clone();
        }
        let declared = self.declared.as_ref().and_then(|font| {
            let code = font
                .text
                .to_unicode
                .iter()
                .filter(|(_, text)| {
                    let mut chars = text.chars();
                    chars.next() == Some(ch) && chars.next().is_none()
                })
                .map(|(code, _)| *code)
                .min()
                .or_else(|| {
                    font.text
                        .encoding
                        .iter()
                        .filter(|(_, c)| **c == ch)
                        .map(|(code, _)| u32::from(*code))
                        .min()
                })?;
            let outline = font.outline(code);
            if outline.is_none() && !ch.is_whitespace() {
                return None;
            }
            Some(FieldGlyph {
                outline,
                advance: font.advance_width(code),
                units_per_em: font.units_per_em(),
            })
        });
        let glyph = declared.unwrap_or_else(|| FieldGlyph {
            outline: self.fallback.outline_for_char(ch),
            advance: self.fallback.advance(ch).unwrap_or(500.0),
            units_per_em: self.fallback.units_per_em(),
        });
        self.glyphs.insert(ch, glyph.clone());
        glyph
    }
}

fn widget_transform(
    rect: (f64, f64, f64, f64),
    ctm: Matrix,
    rotation: i64,
) -> ((f64, f64, f64, f64), Matrix) {
    let (w, h) = (rect.2 - rect.0, rect.3 - rect.1);
    match rotation.rem_euclid(360) {
        90 => (
            (0.0, 0.0, h, w),
            Matrix::new(0.0, 1.0, -1.0, 0.0, rect.2, rect.1).then(&ctm),
        ),
        180 => (
            (0.0, 0.0, w, h),
            Matrix::new(-1.0, 0.0, 0.0, -1.0, rect.2, rect.3).then(&ctm),
        ),
        270 => (
            (0.0, 0.0, h, w),
            Matrix::new(0.0, -1.0, 1.0, 0.0, rect.0, rect.3).then(&ctm),
        ),
        _ => (rect, ctm),
    }
}

fn annotation_transform(ctm: Matrix, rect: (f64, f64, f64, f64), flags: i64) -> Matrix {
    if flags & 16 == 0 {
        return ctm;
    }
    // NoRotate fixes the annotation's upper-left corner at the transformed
    // page position while cancelling page rotation for its own artwork.
    let anchor = ctm.apply(rect.0, rect.3);
    let sx = ctm.a.hypot(ctm.b);
    let sy = ctm.c.hypot(ctm.d);
    Matrix::new(
        sx,
        0.0,
        0.0,
        -sy,
        anchor.0 - sx * rect.0,
        anchor.1 + sy * rect.3,
    )
}

fn markup_baseline(quad: &[f64]) -> ((f64, f64), (f64, f64), (f64, f64), f64) {
    let p = [
        (quad[0], quad[1]),
        (quad[2], quad[3]),
        (quad[4], quad[5]),
        (quad[6], quad[7]),
    ];
    let edge = (p[1].0 - p[0].0, p[1].1 - p[0].1);
    let cross = edge.0 * (p[2].1 - p[1].1) - edge.1 * (p[2].0 - p[1].0);
    let (start, end, opposite) = if cross > 0.0 {
        (
            p[0],
            p[1],
            ((p[2].0 + p[3].0) * 0.5, (p[2].1 + p[3].1) * 0.5),
        )
    } else {
        let dot = (p[3].0 - p[2].0) * edge.0 + (p[3].1 - p[2].1) * edge.1;
        let (start, end) = if dot >= 0.0 {
            (p[2], p[3])
        } else {
            (p[3], p[2])
        };
        (
            start,
            end,
            ((p[0].0 + p[1].0) * 0.5, (p[0].1 + p[1].1) * 0.5),
        )
    };
    let mut normal = (
        opposite.0 - (start.0 + end.0) * 0.5,
        opposite.1 - (start.1 + end.1) * 0.5,
    );
    let height = normal.0.hypot(normal.1).max(1e-6);
    normal.0 /= height;
    normal.1 /= height;
    (start, end, normal, height)
}

fn ellipse(path: &mut Path, rect: (f64, f64, f64, f64)) {
    let (cx, cy) = ((rect.0 + rect.2) * 0.5, (rect.1 + rect.3) * 0.5);
    let (rx, ry) = ((rect.2 - rect.0) * 0.5, (rect.3 - rect.1) * 0.5);
    let k = 0.5522847498307936;
    path.move_to(cx + rx, cy);
    path.curve_to(cx + rx, cy + ry * k, cx + rx * k, cy + ry, cx, cy + ry);
    path.curve_to(cx - rx * k, cy + ry, cx - rx, cy + ry * k, cx - rx, cy);
    path.curve_to(cx - rx, cy - ry * k, cx - rx * k, cy - ry, cx, cy - ry);
    path.curve_to(cx + rx * k, cy - ry, cx + rx, cy - ry * k, cx + rx, cy);
    path.close();
}

fn line_ending(
    name: &str,
    tip: (f64, f64),
    neighbor: (f64, f64),
    size: f64,
) -> Option<(Path, bool)> {
    if name == "None" {
        return None;
    }
    let angle = (tip.1 - neighbor.1).atan2(tip.0 - neighbor.0);
    let transform = Matrix::new(
        angle.cos(),
        angle.sin(),
        -angle.sin(),
        angle.cos(),
        tip.0,
        tip.1,
    );
    let mut path = Path::new();
    let fill = matches!(
        name,
        "ClosedArrow" | "RClosedArrow" | "Square" | "Circle" | "Diamond"
    );
    match name {
        "OpenArrow" | "ClosedArrow" | "ROpenArrow" | "RClosedArrow" => {
            let x = if name.starts_with('R') { size } else { -size };
            path.move_to(x, -size * 0.5);
            path.line_to(0.0, 0.0);
            path.line_to(x, size * 0.5);
            if fill {
                path.close();
            }
        }
        "Square" => path.rect(-size * 0.5, -size * 0.5, size, size),
        "Circle" => ellipse(
            &mut path,
            (-size * 0.5, -size * 0.5, size * 0.5, size * 0.5),
        ),
        "Diamond" => {
            path.move_to(-size * 0.5, 0.0);
            path.line_to(0.0, size * 0.5);
            path.line_to(size * 0.5, 0.0);
            path.line_to(0.0, -size * 0.5);
            path.close();
        }
        "Butt" => {
            path.move_to(0.0, -size * 0.5);
            path.line_to(0.0, size * 0.5);
        }
        "Slash" => {
            path.move_to(-size * 0.25, -size * 0.5);
            path.line_to(size * 0.25, size * 0.5);
        }
        _ => return None,
    }
    Some((path.transform(&transform), fill))
}

fn field_text(value: &PdfObject) -> String {
    match value {
        PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => {
            pdf_ops::metadata::decode_text_string(bytes)
        }
        PdfObject::Array(values) => values.iter().map(field_text).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn standard_annotation_type(subtype: &str) -> bool {
    matches!(
        subtype,
        "Text"
            | "Link"
            | "FreeText"
            | "Line"
            | "Square"
            | "Circle"
            | "Polygon"
            | "PolyLine"
            | "Highlight"
            | "Underline"
            | "Squiggly"
            | "StrikeOut"
            | "Caret"
            | "Stamp"
            | "Ink"
            | "Popup"
            | "FileAttachment"
            | "Sound"
            | "Movie"
            | "Widget"
            | "Screen"
            | "PrinterMark"
            | "TrapNet"
            | "Watermark"
            | "3D"
            | "Redact"
            | "Projection"
            | "RichMedia"
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::{doc_with_content, pixel};
    use super::*;
    use pdf_core::parser::Parser;
    use pdf_core::stream::PdfStream;

    fn object(s: &str) -> PdfObject {
        Parser::new(s.as_bytes()).parse_object().unwrap()
    }
    fn set_page(doc: &mut PdfDocument, key: &str, value: PdfObject) {
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(key.into(), value);
        doc.set_object(id, PdfObject::Dictionary(page));
    }
    fn annotations(doc: &mut PdfDocument, source: &str) {
        set_page(doc, "Annots", object(source));
    }
    fn render(doc: &PdfDocument) -> RenderedPage {
        render_page(doc, 0, RenderOptions::default()).unwrap()
    }
    fn black_bounds(page: &RenderedPage) -> (u32, u32, u32, u32) {
        let mut b = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..page.height {
            for x in 0..page.width {
                let c = pixel(page, x, y);
                if c.0 < 100 && c.1 < 100 && c.2 < 100 {
                    b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
        }
        b
    }

    #[test]
    fn geometric_fallbacks_draw_fill_ellipse_and_ink() {
        let mut doc = doc_with_content("", [0, 0, 100, 80]);
        annotations(&mut doc,"[<< /Subtype /Square /Rect [10 10 30 30] /C [1 0 0] /IC [0 1 0] /BS << /W 2 /S /D /D [3 3] >> >> << /Subtype /Circle /Rect [40 10 60 30] /C [0 0 1] /IC [0 0 1] >> << /Subtype /Ink /Rect [10 40 70 70] /C [0 0 0] /BS << /W 2 >> /InkList [[10 40 30 60 50 40] [60 40 60 60]] >>]");
        let page = render(&doc);
        assert_eq!(pixel(&page, 20, 60), (0, 255, 0));
        assert_eq!(pixel(&page, 50, 60), (0, 0, 255));
        assert_eq!(pixel(&page, 40, 50), (255, 255, 255));
        assert!(pixel(&page, 60, 30).0 < 30);
        let edge = (12..28).map(|x| pixel(&page, x, 69)).collect::<Vec<_>>();
        assert!(edge.iter().any(|p| p.0 > 200 && p.1 < 50));
        assert!(edge.iter().any(|p| p.1 > 200));
    }

    #[test]
    fn rotated_markup_follows_the_quad_baseline() {
        let mut doc = doc_with_content("", [0, 0, 80, 80]);
        annotations(&mut doc,"[<< /Subtype /Underline /Rect [10 10 30 60] /C [1 0 0] /QuadPoints [20 10 20 60 30 10 30 60] >>]");
        let page = render(&doc);
        assert!(pixel(&page, 30, 40).1 < 200);
        assert_eq!(pixel(&page, 20, 40), (255, 255, 255));
        let adobe = markup_baseline(&[10., 30., 40., 30., 10., 20., 40., 20.]);
        let perimeter = markup_baseline(&[10., 20., 40., 20., 40., 30., 10., 30.]);
        assert_eq!(adobe, perimeter);
    }

    #[test]
    fn no_rotate_keeps_appearance_upright_around_its_upper_left_anchor() {
        let mut doc = doc_with_content("", [0, 0, 100, 80]);
        set_page(&mut doc, "Rotate", PdfObject::Integer(90));
        let ap = doc.add_object(PdfObject::Stream(PdfStream::new(
            object("<< /Subtype /Form /BBox [0 0 30 10] >>")
                .as_dict()
                .unwrap()
                .clone(),
            b"1 0 0 rg 0 0 30 10 re f".to_vec(),
        )));
        annotations(
            &mut doc,
            &format!(
                "[<< /Subtype /Stamp /F 16 /Rect [10 20 40 30] /AP << /N {} 0 R >> >>]",
                ap.number
            ),
        );
        let page = render(&doc);
        assert_eq!(pixel(&page, 50, 15), (255, 0, 0));
        assert_eq!(pixel(&page, 25, 30), (255, 255, 255));
    }

    #[test]
    fn comb_field_centers_one_glyph_in_each_declared_cell() {
        let mut doc = doc_with_content("", [0, 0, 100, 40]);
        annotations(&mut doc,"[<< /Subtype /Widget /FT /Tx /Ff 16777216 /MaxLen 4 /Rect [10 10 90 30] /Border [0 0 0] /DA (/F 12 Tf) /V (iiii) >>]");
        let page = render(&doc);
        let occupied = (0..100)
            .filter(|x| (10..30).any(|y| pixel(&page, *x, y).0 < 100))
            .collect::<Vec<_>>();
        for center in [20i32, 40, 60, 80] {
            assert!(
                occupied.iter().any(|x| (*x as i32 - center).abs() <= 2),
                "{occupied:?}"
            );
        }
        assert!(occupied.iter().all(|x| [20i32, 40, 60, 80]
            .iter()
            .any(|c| (*x as i32 - *c).abs() <= 2)));
        assert!(!page
            .warnings
            .iter()
            .any(|w| w.contains("ordinary text spacing")));
    }

    #[test]
    fn widget_rotation_changes_text_orientation_and_stays_inside_rect() {
        let mut doc = doc_with_content("", [0, 0, 60, 100]);
        annotations(&mut doc,"[<< /Subtype /Widget /FT /Tx /Rect [10 10 40 90] /Border [0 0 0] /DA (/F 12 Tf) /MK << /R 90 >> /V (ABCD) >>]");
        let page = render(&doc);
        let b = black_bounds(&page);
        assert!(b.0 >= 10 && b.2 < 40 && b.1 >= 10 && b.3 < 90, "{b:?}");
        assert!(b.3 - b.1 > 2 * (b.2 - b.0), "{b:?}");
        assert!(!page.warnings.iter().any(|w| w.contains("unrotated")));
    }

    #[test]
    fn pushbutton_draws_its_icon_and_caption_in_distinct_regions() {
        let mut doc = doc_with_content("", [0, 0, 100, 60]);
        let icon = doc.add_object(PdfObject::Stream(PdfStream::new(
            object("<< /Subtype /Form /BBox [0 0 10 10] >>")
                .as_dict()
                .unwrap()
                .clone(),
            b"1 0 0 rg 0 0 10 10 re f".to_vec(),
        )));
        annotations(&mut doc,&format!("[<< /Subtype /Widget /FT /Btn /Ff 65536 /Rect [10 10 90 50] /Border [0 0 0] /MK << /CA (GO) /TP 2 /I {} 0 R >> >>]",icon.number));
        let page = render(&doc);
        assert_eq!(pixel(&page, 50, 20), (255, 0, 0));
        let b = black_bounds(&page);
        assert!(b.1 >= 38 && b.3 < 50, "{b:?}");
        assert!(!page
            .warnings
            .iter()
            .any(|w| w.contains("caption and icon are not drawn")));
    }

    #[test]
    fn choice_list_honours_top_item_and_selected_indices() {
        let mut doc = doc_with_content("", [0, 0, 80, 60]);
        annotations(&mut doc,"[<< /Subtype /Widget /FT /Ch /Rect [10 10 70 50] /DA (/F 10 Tf) /Border [0 0 0] /Opt [(First) [(b) (Second)] (Third)] /TI 1 /I [2] /V (b) >>]");
        let page = render(&doc);
        assert_eq!(pixel(&page, 65, 16), (255, 255, 255));
        let selected = pixel(&page, 65, 29);
        assert!(selected.2 > 150 && selected.0 < 80, "{selected:?}");
        assert_eq!(pixel(&page, 65, 45), (255, 255, 255));
    }

    #[test]
    fn widget_uses_declared_font_widths_for_right_alignment() {
        let mut doc = doc_with_content("", [0, 0, 100, 40]);
        let catalog_id = doc.xref.trailer.get("Root").unwrap().as_ref().unwrap();
        let mut catalog = doc.catalog().unwrap().clone();
        catalog.insert("AcroForm".into(),object("<< /DR << /Font << /Wide << /Subtype /Type1 /BaseFont /Helvetica /FirstChar 65 /Widths [1000] >> >> >> >>"));
        doc.set_object(catalog_id, PdfObject::Dictionary(catalog));
        let parent = doc.add_object(object("<< /FT /Tx /V (AA) /DA (/Wide 10 Tf) /Q 2 >>"));
        annotations(
            &mut doc,
            &format!(
                "[<< /Subtype /Widget /Parent {} 0 R /Rect [10 10 80 30] /Border [0 0 0] >>]",
                parent.number
            ),
        );
        let page = render(&doc);
        let b = black_bounds(&page);
        // 80 - 1px padding - (2 * 10pt declared advance) = 59pt.
        assert!((59..=61).contains(&b.0), "{b:?}");
        assert!((74..=76).contains(&b.2), "{b:?}");
    }

    #[test]
    fn widget_border_styles_draw_dashes_underlines_and_bevels() {
        let mut doc = doc_with_content("", [0, 0, 100, 40]);
        annotations(&mut doc,"[<< /Subtype /Widget /FT /Tx /Rect [5 5 25 35] /MK << /BC [0 0 0] >> /BS << /W 2 /S /D /D [3 3] >> >> << /Subtype /Widget /FT /Tx /Rect [35 5 55 35] /MK << /BC [1 0 0] >> /BS << /W 2 /S /U >> >> << /Subtype /Widget /FT /Tx /Rect [65 5 95 35] /MK << /BC [0 0 0] >> /BS << /W 3 /S /B >> >>]");
        let page = render(&doc);
        let dashed = (8..22).map(|x| pixel(&page, x, 34).0).collect::<Vec<_>>();
        assert!(
            dashed.iter().any(|v| *v < 50) && dashed.iter().any(|v| *v > 200),
            "{dashed:?}"
        );
        assert_eq!(pixel(&page, 45, 34), (255, 0, 0));
        assert_eq!(pixel(&page, 45, 5), (255, 255, 255));
        assert!(pixel(&page, 80, 6).0 > 200);
        assert!(pixel(&page, 80, 33).0 < 100);
    }
}
