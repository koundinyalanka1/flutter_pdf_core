//! Pattern painting and ISO 32000-1 8.7 shadings (types 1 through 7).
//! Functions are parsed once; mesh/function workloads have explicit bounds.
use super::*;
use pdf_core::stream::PdfStream;
#[path = "page_functions.rs"]
mod functions;
use functions::ColorFunction;
#[path = "page_mesh.rs"]
mod mesh;

pub(super) fn scalar_function_table(doc: &PdfDocument, object: &PdfObject) -> Option<Vec<f64>> {
    let f = ColorFunction::load(doc, object, 0, &mut 4096)?;
    if f.inputs() != 1 || f.outputs() != 1 {
        return None;
    }
    (0..=255)
        .map(|i| Some(f.evaluate(i as f64 / 255.0)?[0].clamp(0.0, 1.0)))
        .collect()
}

impl Renderer<'_> {
    pub(super) fn is_pattern_space(&self, op: &Operation, resources: &Dictionary) -> bool {
        let Some(name) = op.operands.first().and_then(PdfObject::as_name) else {
            return false;
        };
        if name == "Pattern" {
            return true;
        }
        match self.resource_object(resources, "ColorSpace", name) {
            Some(PdfObject::Name(space)) => space == "Pattern",
            Some(PdfObject::Array(items)) => {
                items
                    .first()
                    .map(|o| self.doc.resolve_value(o))
                    .as_ref()
                    .and_then(PdfObject::as_name)
                    == Some("Pattern")
            }
            _ => false,
        }
    }

    pub(super) fn paint_color(
        &mut self,
        path: &Path,
        rule: FillRule,
        fill: bool,
        resources: &Dictionary,
        state: &GraphicsState,
    ) {
        if !self.visible {
            return;
        }
        self.configure_canvas(state);
        let (pattern_space, pattern, color, alpha) = if fill {
            (
                state.fill_pattern_space,
                &state.fill_pattern,
                state.fill,
                state.fill_alpha,
            )
        } else {
            (
                state.stroke_pattern_space,
                &state.stroke_pattern,
                state.stroke,
                state.stroke_alpha,
            )
        };
        if !pattern_space {
            self.canvas
                .fill_path(path, color, rule, alpha, state.clip.as_deref());
            return;
        }
        let Some(object) = pattern
            .as_ref()
            .and_then(|name| self.resource_object(resources, "Pattern", name))
        else {
            self.warn("pattern fill skipped: missing pattern resource");
            return;
        };
        if self.depth > 12 {
            self.warn("pattern skipped: recursive content exceeds renderer limit");
            return;
        }
        let Some(dict) = object.as_dict() else {
            self.warn("pattern skipped: invalid dictionary");
            return;
        };
        let mut inner = state.clone();
        inner.fill = color;
        inner.stroke = color;
        inner.fill_alpha = alpha;
        inner.stroke_alpha = alpha;
        inner.fill_pattern_space = false;
        inner.stroke_pattern_space = false;
        inner.fill_pattern = None;
        inner.stroke_pattern = None;
        inner.ctm = matrix(self.doc, dict).then(&state.pattern_ctm);
        self.clip_path(path, rule, &mut inner);
        match dict.get("PatternType").and_then(PdfObject::as_i64) {
            Some(2) => {
                if let Some(ext) = dict.get("ExtGState") {
                    if let Some(ext) = self.doc.resolve_dict(ext).cloned() {
                        self.apply_ext_gstate_dict(&ext, resources, &mut inner);
                    } else {
                        self.warn("shading pattern: invalid ExtGState dictionary");
                    }
                }
                if let Some(shading) = dict.get("Shading").map(|v| self.doc.resolve_value(v)) {
                    self.draw_shading(&shading, resources, &inner, true);
                } else {
                    self.warn("pattern skipped: missing shading dictionary");
                }
            }
            Some(1) => {
                if let PdfObject::Stream(stream) = &object {
                    self.draw_tiles(stream, resources, &inner);
                } else {
                    self.warn("tiling pattern skipped: missing content stream");
                }
            }
            _ => self.warn("pattern skipped: unsupported pattern type"),
        }
    }

    pub(super) fn clip_path(&mut self, path: &Path, rule: FillRule, state: &mut GraphicsState) {
        let mask = self.canvas.rasterize_mask(path, rule);
        state.clip = Some(Rc::new(match state.clip.as_deref() {
            Some(existing) => existing.intersect(&mask),
            None => mask,
        }));
    }

    fn draw_tiles(&mut self, stream: &PdfStream, resources: &Dictionary, state: &GraphicsState) {
        let dict = &stream.dictionary;
        let Some(bbox) = array_rect(self.doc, dict, "BBox") else {
            self.warn("tiling pattern skipped: missing bounding box");
            return;
        };
        let xstep = dict.get("XStep").and_then(as_number).unwrap_or(0.0).abs();
        let ystep = dict.get("YStep").and_then(as_number).unwrap_or(0.0).abs();
        if !xstep.is_finite() || !ystep.is_finite() || xstep < 1e-6 || ystep < 1e-6 {
            self.warn("tiling pattern skipped: invalid tile spacing");
            return;
        }
        let paint_type = dict.get("PaintType").and_then(PdfObject::as_i64);
        if !matches!(paint_type, Some(1 | 2)) {
            self.warn("tiling pattern skipped: invalid PaintType");
            return;
        }
        let Some(inverse) = state.ctm.invert() else {
            self.warn("tiling pattern skipped: singular transform");
            return;
        };
        let Some(clip) = state.clip.as_deref() else {
            return;
        };
        let mut min_x = self.canvas.width;
        let mut max_x = 0;
        let mut min_y = self.canvas.height;
        let mut max_y = 0;
        for (i, coverage) in clip.data.iter().enumerate() {
            if *coverage != 0 {
                min_x = min_x.min(i % clip.width);
                max_x = max_x.max(i % clip.width + 1);
                min_y = min_y.min(i / clip.width);
                max_y = max_y.max(i / clip.width + 1);
            }
        }
        if max_x == 0 || max_y == 0 {
            return;
        }
        let visible = bounds(
            inverse,
            (min_x as f64, min_y as f64, max_x as f64, max_y as f64),
        );
        let ix0 = ((visible.0 - bbox.2) / xstep).floor();
        let ix1 = ((visible.2 - bbox.0) / xstep).ceil();
        let iy0 = ((visible.1 - bbox.3) / ystep).floor();
        let iy1 = ((visible.3 - bbox.1) / ystep).ceil();
        let count = (ix1 - ix0 + 1.0) * (iy1 - iy0 + 1.0);
        // Each tile needs an independent clip; bound both tile count and work
        // rather than allowing untrusted tiny steps to stall a phone.
        if !count.is_finite()
            || count > 8192.0
            || self.pattern_pixels + count * self.canvas.width as f64 * self.canvas.height as f64
                > 256_000_000.0
        {
            self.warn("tiling pattern skipped: tile density exceeds renderer limit");
            return;
        }
        self.pattern_pixels += count * self.canvas.width as f64 * self.canvas.height as f64;
        let Ok(data) = self.doc.stream_data(stream) else {
            self.warn("tiling pattern skipped: content cannot be decoded");
            return;
        };
        let local_resources = dict
            .get("Resources")
            .and_then(|o| self.doc.resolve_dict(o))
            .cloned()
            .unwrap_or_else(|| resources.clone());
        let outer_fonts = std::mem::take(&mut self.fonts);
        self.depth += 1;
        for iy in iy0 as i64..=iy1 as i64 {
            for ix in ix0 as i64..=ix1 as i64 {
                let mut tile = state.clone();
                tile.ctm = Matrix::translate(ix as f64 * xstep, iy as f64 * ystep).then(&state.ctm);
                tile.pattern_ctm = tile.ctm;
                let mut path = Path::new();
                path.rect(bbox.0, bbox.1, bbox.2 - bbox.0, bbox.3 - bbox.1);
                self.clip_path(&path.transform(&tile.ctm), FillRule::NonZero, &mut tile);
                let _ = self.run(&data, &local_resources, &mut tile);
            }
        }
        self.depth -= 1;
        self.fonts = outer_fonts;
    }

    pub(super) fn draw_shading(
        &mut self,
        object: &PdfObject,
        resources: &Dictionary,
        state: &GraphicsState,
        paint_background: bool,
    ) {
        self.configure_canvas(state);
        let Some(dict) = object.as_dict() else {
            self.warn("shading skipped: invalid dictionary");
            return;
        };
        let kind = dict
            .get("ShadingType")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0);
        if !(1..=7).contains(&kind) {
            self.warn(format!("shading skipped: invalid type {kind}"));
            return;
        }
        let mut space = dict
            .get("ColorSpace")
            .map(|o| self.doc.resolve_value(o))
            .unwrap_or(PdfObject::Null);
        if let Some(name) = space.as_name() {
            if let Some(resolved) = self.resource_object(resources, "ColorSpace", name) {
                space = resolved;
            }
        }
        let components = match space.as_name() {
            Some("DeviceGray" | "G") => 1,
            Some("DeviceRGB" | "RGB") => 3,
            Some("DeviceCMYK" | "CMYK") => 4,
            _ => {
                self.warn("shading skipped: colour space is not DeviceGray/RGB/CMYK");
                return;
            }
        };
        let mut local = state.clone();
        if let Some(bbox) = array_rect(self.doc, dict, "BBox") {
            let mut p = Path::new();
            p.rect(bbox.0, bbox.1, bbox.2 - bbox.0, bbox.3 - bbox.1);
            self.clip_path(&p.transform(&state.ctm), FillRule::NonZero, &mut local);
        }
        if paint_background {
            if let Some(background) = color_from(&numbers(self.doc, dict.get("Background"))) {
                let mut full = Path::new();
                full.rect(
                    0.0,
                    0.0,
                    self.canvas.width as f64,
                    self.canvas.height as f64,
                );
                self.canvas.fill_path(
                    &full,
                    background,
                    FillRule::NonZero,
                    local.fill_alpha,
                    local.clip.as_deref(),
                );
            }
        }
        if kind == 1 {
            self.draw_function_shading(dict, components, &local);
            return;
        }
        if kind >= 4 {
            self.draw_mesh(object, kind, components, &local);
            return;
        }
        let coords = numbers(self.doc, dict.get("Coords"));
        if coords.len() != if kind == 2 { 4 } else { 6 } || coords.iter().any(|v| !v.is_finite()) {
            self.warn("shading skipped: invalid coordinates");
            return;
        }
        if kind == 3 && (coords[2] < 0.0 || coords[5] < 0.0) {
            self.warn("radial shading skipped: negative radius");
            return;
        }
        let Some(function) = dict
            .get("Function")
            .and_then(|o| ColorFunction::load(self.doc, o, 0, &mut 4096))
        else {
            self.warn("shading skipped: unsupported or malformed colour function");
            return;
        };
        if function.inputs() != 1 || function.outputs() != components {
            self.warn("shading skipped: colour function component mismatch");
            return;
        }
        let domain = numbers(self.doc, dict.get("Domain"));
        let (d0, d1) = if domain.len() == 2 {
            (domain[0], domain[1])
        } else {
            (0.0, 1.0)
        };
        if !d0.is_finite() || !d1.is_finite() || d0 > d1 {
            self.warn("shading skipped: invalid domain");
            return;
        }
        let extend = match dict.get("Extend").map(|o| self.doc.resolve_value(o)) {
            Some(PdfObject::Array(items)) => (
                matches!(items.first(), Some(PdfObject::Bool(true))),
                matches!(items.get(1), Some(PdfObject::Bool(true))),
            ),
            _ => (false, false),
        };
        let Some(inverse) = state.ctm.invert() else {
            return;
        };
        // Colour evaluation is independent of position. A small lookup table
        // avoids allocating function outputs for every pixel of a gradient.
        let palette: Option<Vec<Rgb>> = (0..=1024)
            .map(|i| {
                let values = function.evaluate(d0 + (d1 - d0) * i as f64 / 1024.0)?;
                if values.iter().any(|n| !n.is_finite()) {
                    return None;
                }
                color_from(&values)
            })
            .collect();
        let Some(palette) = palette else {
            self.warn("shading skipped: invalid colour function output");
            return;
        };
        for y in 0..self.canvas.height {
            for x in 0..self.canvas.width {
                if local
                    .clip
                    .as_deref()
                    .is_some_and(|m| m.data[y * m.width + x] == 0)
                {
                    continue;
                }
                let (px, py) = inverse.apply(x as f64 + 0.5, y as f64 + 0.5);
                let t = if kind == 2 {
                    axial_parameter(&coords, px, py)
                } else {
                    radial_parameter(&coords, px, py, extend)
                };
                let Some(t) = t else {
                    continue;
                };
                if (t < 0.0 && !extend.0) || (t > 1.0 && !extend.1) {
                    continue;
                }
                let color = palette[(t.clamp(0.0, 1.0) * 1024.0).round() as usize];
                self.canvas
                    .blend(x, y, color, local.fill_alpha, local.clip.as_deref());
            }
        }
    }

    fn draw_function_shading(
        &mut self,
        dict: &Dictionary,
        components: usize,
        state: &GraphicsState,
    ) {
        let domain = if dict.contains_key("Domain") {
            numbers(self.doc, dict.get("Domain"))
        } else {
            vec![0.0, 1.0, 0.0, 1.0]
        };
        if domain.len() != 4
            || domain.iter().any(|n| !n.is_finite())
            || domain[0] > domain[1]
            || domain[2] > domain[3]
        {
            self.warn("function shading skipped: invalid domain");
            return;
        }
        let Some(function) = dict
            .get("Function")
            .and_then(|o| ColorFunction::load(self.doc, o, 0, &mut 4096))
        else {
            self.warn("function shading skipped: malformed colour function");
            return;
        };
        if function.inputs() != 2 || function.outputs() != components {
            self.warn("function shading skipped: colour function component mismatch");
            return;
        }
        let Some(inverse) = matrix(self.doc, dict).then(&state.ctm).invert() else {
            return;
        };
        let mut budget = 64_000_000;
        for y in 0..self.canvas.height {
            for x in 0..self.canvas.width {
                if state
                    .clip
                    .as_deref()
                    .is_some_and(|m| m.data[y * m.width + x] == 0)
                {
                    continue;
                }
                let p = inverse.apply(x as f64 + 0.5, y as f64 + 0.5);
                if p.0 < domain[0] || p.0 > domain[1] || p.1 < domain[2] || p.1 > domain[3] {
                    continue;
                }
                let Some(values) = function.evaluate_inputs_bounded(&[p.0, p.1], &mut budget)
                else {
                    self.warn(
                        "function shading stopped: invalid output or evaluation budget exceeded",
                    );
                    return;
                };
                if let Some(color) = color_from(&values) {
                    self.canvas
                        .blend(x, y, color, state.fill_alpha, state.clip.as_deref());
                }
            }
        }
    }

    pub(super) fn resolve_inline_color_space(
        &self,
        stream: &mut PdfStream,
        resources: &Dictionary,
    ) {
        for (short, long) in [
            ("W", "Width"),
            ("H", "Height"),
            ("BPC", "BitsPerComponent"),
            ("CS", "ColorSpace"),
            ("D", "Decode"),
            ("DP", "DecodeParms"),
            ("F", "Filter"),
            ("IM", "ImageMask"),
            ("I", "Interpolate"),
        ] {
            if let Some(value) = stream.dictionary.remove(short) {
                stream.dictionary.insert(long.into(), value);
            }
        }
        if let Some(PdfObject::Name(name)) = stream.dictionary.get("ColorSpace") {
            if let Some(space) = self.resource_object(resources, "ColorSpace", name) {
                stream.dictionary.insert("ColorSpace".into(), space);
            }
        }
    }
}

pub(super) fn matrix(doc: &PdfDocument, dict: &Dictionary) -> Matrix {
    let v = numbers(doc, dict.get("Matrix"));
    if v.len() == 6 && v.iter().all(|n| n.is_finite()) {
        Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5])
    } else {
        Matrix::IDENTITY
    }
}

pub(super) fn bounds(m: Matrix, b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let corners = [
        m.apply(b.0, b.1),
        m.apply(b.0, b.3),
        m.apply(b.2, b.1),
        m.apply(b.2, b.3),
    ];
    corners.iter().fold(
        (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |r, p| (r.0.min(p.0), r.1.min(p.1), r.2.max(p.0), r.3.max(p.1)),
    )
}

pub(super) fn numbers(doc: &PdfDocument, object: Option<&PdfObject>) -> Vec<f64> {
    match object.map(|o| doc.resolve_value(o)) {
        Some(PdfObject::Array(items)) => items
            .iter()
            .map(|o| as_number(&doc.resolve_value(o)))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn axial_parameter(c: &[f64], x: f64, y: f64) -> Option<f64> {
    let (dx, dy) = (c[2] - c[0], c[3] - c[1]);
    let length2 = dx * dx + dy * dy;
    (length2 > 1e-20).then(|| ((x - c[0]) * dx + (y - c[1]) * dy) / length2)
}

fn radial_parameter(c: &[f64], x: f64, y: f64, extend: (bool, bool)) -> Option<f64> {
    let (dx, dy, dr) = (c[3] - c[0], c[4] - c[1], c[5] - c[2]);
    let (px, py) = (x - c[0], y - c[1]);
    let a = dx * dx + dy * dy - dr * dr;
    let b = -2.0 * (px * dx + py * dy + c[2] * dr);
    let cc = px * px + py * py - c[2] * c[2];
    let roots = if a.abs() < 1e-12 {
        if b.abs() < 1e-12 {
            return None;
        }
        [-cc / b, -cc / b]
    } else {
        let d = b * b - 4.0 * a * cc;
        if d < 0.0 {
            return None;
        }
        [(-b + d.sqrt()) / (2.0 * a), (-b - d.sqrt()) / (2.0 * a)]
    };
    // ISO 32000: overlapping circles paint in increasing t order.
    roots
        .into_iter()
        .filter(|t| {
            t.is_finite()
                && c[2] + t * dr >= 0.0
                && (*t >= 0.0 || extend.0)
                && (*t <= 1.0 || extend.1)
        })
        .max_by(f64::total_cmp)
}

#[cfg(test)]
#[path = "page_paints_tests.rs"]
mod tests;
