//! Image XObjects, drawn by area sampling rather than point sampling.
//!
//! A scan is usually two or three times the resolution of the screen showing
//! it. Taking one source pixel per device pixel, as this used to, kept a
//! fraction of the scan and dropped the rest: thin strokes broke into dots
//! and neighbouring letters came out at different weights. Each device pixel
//! is now the average of the image area it covers, which is what PDFium does.
//! An enlarged image keeps crisp pixel edges, again as PDFium draws it, unless
//! the document asks for `/Interpolate`.
use super::*;
use crate::image::{AlphaPlane, DecodedImage};
use pdf_core::stream::PdfStream;

/// Skew, in device pixels across the whole image, small enough to treat the
/// image as lined up with the pixel grid.
const AXIS_TOLERANCE: f64 = 0.25;

impl Renderer<'_> {
    /// Paint an image into the unit square mapped by the CTM.
    pub(super) fn draw_image(&mut self, stream: &PdfStream, state: &GraphicsState) {
        if !self.visible {
            return;
        }
        self.configure_canvas(state);
        let Some(image) = decode_image(self.doc, stream) else {
            let codec = image_codec_name(self.doc, stream);
            self.warn(format!(
                "image skipped: {codec} is unsupported or its data is damaged"
            ));
            return;
        };
        if image.is_stencil && state.fill_pattern_space {
            self.warn(
                "pattern-coloured stencil image skipped: this paint combination is unsupported",
            );
            return;
        }
        if image.width == 0 || image.height == 0 {
            return;
        }
        let interpolate = matches!(
            stream
                .dictionary
                .get("Interpolate")
                .map(|v| self.doc.resolve_value(v)),
            Some(PdfObject::Bool(true))
        );
        let source = Source::new(&image);
        // A mask of another size than the image is sampled at its own
        // resolution, so a fine text mask over coarse colour stays sharp.
        let mask = image.mask.as_ref().map(Source::coverage);
        let paint = Paint {
            stencil_color: image.is_stencil.then_some(state.fill),
            alpha: state.fill_alpha,
            clip: state.clip.clone(),
        };
        let plans =
            AxisPlan::new(&state.ctm, image.width, image.height).and_then(|plan| match &mask {
                Some(mask) => Some((
                    plan,
                    Some(AxisPlan::new(&state.ctm, mask.width, mask.height)?),
                )),
                None => Some((plan, None)),
            });
        match plans {
            Some((plan, mask_plan)) => {
                let mask = mask.as_ref().zip(mask_plan.as_ref());
                self.draw_axis_aligned(&source, &plan, mask, interpolate, &paint)
            }
            None => self.draw_transformed(&source, mask.as_ref(), &state.ctm, interpolate, &paint),
        }
    }

    /// The common case — upright, flipped, or a quarter turn — as an exact
    /// separable box filter: rows of the image are blended into one row of
    /// the right height, then that row into device pixels.
    fn draw_axis_aligned(
        &mut self,
        source: &Source,
        plan: &AxisPlan,
        mask: Option<(&Source, &AxisPlan)>,
        interpolate: bool,
        paint: &Paint,
    ) {
        let (width, height) = (self.canvas.width, self.canvas.height);
        let Some(mut color) = AxisResampler::new(source, plan, width, height, interpolate) else {
            return;
        };
        let mut mask = match mask {
            Some((mask, plan)) => {
                let Some(resampler) = AxisResampler::new(mask, plan, width, height, interpolate)
                else {
                    return;
                };
                Some(resampler)
            }
            None => None,
        };
        let rows: Vec<usize> = color.rows.pixels().map(|(y, _)| y).collect();
        let columns: Vec<usize> = color.columns.pixels().map(|(x, _)| x).collect();
        for y in rows {
            color.blend_row(y);
            if let Some(mask) = &mut mask {
                mask.blend_row(y);
            }
            for &x in &columns {
                let mut texel = color.sample(x);
                if let Some(mask) = &mask {
                    let coverage = mask.sample(x)[3];
                    texel = texel.map(|c| c * coverage);
                }
                self.blend_texel(x, y, texel, paint);
            }
        }
    }

    /// Rotation by anything but a quarter turn, or skew: the image is first
    /// boxed down to roughly device resolution, then each device pixel
    /// averages a few samples of it — enough to keep a turned scan legible
    /// and its edges smooth.
    fn draw_transformed(
        &mut self,
        source: &Source,
        mask: Option<&Source>,
        ctm: &Matrix,
        interpolate: bool,
        paint: &Paint,
    ) {
        let Some(inverse) = ctm.invert() else {
            return;
        };
        let corners = [
            ctm.apply(0.0, 0.0),
            ctm.apply(1.0, 0.0),
            ctm.apply(0.0, 1.0),
            ctm.apply(1.0, 1.0),
        ];
        let min_x = corners.iter().map(|c| c.0).fold(f64::MAX, f64::min);
        let max_x = corners.iter().map(|c| c.0).fold(f64::MIN, f64::max);
        let min_y = corners.iter().map(|c| c.1).fold(f64::MAX, f64::min);
        let max_y = corners.iter().map(|c| c.1).fold(f64::MIN, f64::max);
        if ![min_x, max_x, min_y, max_y].iter().all(|v| v.is_finite()) {
            return;
        }
        let x0 = min_x.floor().max(0.0) as usize;
        let y0 = min_y.floor().max(0.0) as usize;
        let x1 = (max_x.ceil().min(self.canvas.width as f64)).max(0.0) as usize;
        let y1 = (max_y.ceil().min(self.canvas.height as f64)).max(0.0) as usize;

        // Each source is boxed down to about device resolution on its own,
        // and `residual` is what is left of the reduction for the samples.
        let mut residual = 1.0f64;
        let mut reduce = |source: &Source| -> Option<Source<'static>> {
            // Source pixels per device pixel along each image axis.
            let along_x = source.width as f64 / ctm.a.hypot(ctm.b).max(1e-9);
            let along_y = source.height as f64 / ctm.c.hypot(ctm.d).max(1e-9);
            let factor_x = (along_x.floor() as usize).clamp(1, source.width);
            let factor_y = (along_y.floor() as usize).clamp(1, source.height);
            residual = residual
                .max(along_x / factor_x as f64)
                .max(along_y / factor_y as f64);
            (factor_x > 1 || factor_y > 1).then(|| source.reduced(factor_x, factor_y))
        };
        let reduced = reduce(source);
        let source = reduced.as_ref().unwrap_or(source);
        let reduced_mask = mask.and_then(&mut reduce);
        let mask = reduced_mask.as_ref().or(mask);
        let samples = (residual.ceil() as usize).clamp(2, 4);
        let step = 1.0 / samples as f64;
        // Image space runs top-down, the unit square bottom-up.
        let sample = |source: &Source, u: f64, v: f64| -> [f32; 4] {
            let (w, h) = (source.width as f64, source.height as f64);
            if interpolate {
                source.bilinear(u * w, (1.0 - v) * h)
            } else {
                source.texel(
                    ((u * w) as usize).min(source.width - 1),
                    (((1.0 - v) * h) as usize).min(source.height - 1),
                )
            }
        };

        for py in y0..y1 {
            for px in x0..x1 {
                let mut sum = [0f32; 4];
                for sy in 0..samples {
                    for sx in 0..samples {
                        let (u, v) = inverse.apply(
                            px as f64 + (sx as f64 + 0.5) * step,
                            py as f64 + (sy as f64 + 0.5) * step,
                        );
                        if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                            continue;
                        }
                        let mut texel = sample(source, u, v);
                        if let Some(mask) = mask {
                            let coverage = sample(mask, u, v)[3];
                            texel = texel.map(|c| c * coverage);
                        }
                        for c in 0..4 {
                            sum[c] += texel[c];
                        }
                    }
                }
                let count = (samples * samples) as f32;
                self.blend_texel(px, py, sum.map(|c| c / count), paint);
            }
        }
    }

    /// Composite one premultiplied sample.
    fn blend_texel(&mut self, x: usize, y: usize, texel: [f32; 4], paint: &Paint) {
        let alpha = texel[3];
        if alpha <= 1e-4 {
            return;
        }
        let color = match paint.stencil_color {
            Some(color) => color,
            None => Rgb::new(texel[0] / alpha, texel[1] / alpha, texel[2] / alpha),
        };
        self.canvas.blend(
            x,
            y,
            color,
            alpha.min(1.0) * paint.alpha,
            paint.clip.as_deref(),
        );
    }
}

/// What an image paints with, besides its own samples.
struct Paint {
    /// The fill colour for a stencil mask, whose samples are only coverage.
    stencil_color: Option<Rgb>,
    alpha: f32,
    clip: crate::canvas::ClipMask,
}

/// Image samples as premultiplied RGBA, 0..1.
struct Source<'a> {
    width: usize,
    height: usize,
    pixels: Pixels<'a>,
}

enum Pixels<'a> {
    Decoded(&'a DecodedImage),
    /// A mask kept at its own resolution: only the alpha channel counts.
    Coverage(&'a AlphaPlane),
    /// Boxed down for a heavily reduced, turned image.
    Reduced(Vec<[f32; 4]>),
}

impl<'a> Source<'a> {
    fn new(image: &'a DecodedImage) -> Source<'a> {
        Source {
            width: image.width,
            height: image.height,
            pixels: Pixels::Decoded(image),
        }
    }

    fn coverage(plane: &'a AlphaPlane) -> Source<'a> {
        Source {
            width: plane.width,
            height: plane.height,
            pixels: Pixels::Coverage(plane),
        }
    }

    fn texel(&self, x: usize, y: usize) -> [f32; 4] {
        let index = y * self.width + x;
        match &self.pixels {
            Pixels::Reduced(data) => data[index],
            Pixels::Coverage(plane) => [0.0, 0.0, 0.0, f32::from(plane.data[index]) / 255.0],
            Pixels::Decoded(image) => {
                let alpha = image
                    .alpha
                    .get(index)
                    .map_or(1.0, |&a| f32::from(a) / 255.0);
                if image.is_stencil {
                    return [0.0, 0.0, 0.0, alpha];
                }
                let rgb = &image.rgb[index * 3..index * 3 + 3];
                [
                    f32::from(rgb[0]) / 255.0 * alpha,
                    f32::from(rgb[1]) / 255.0 * alpha,
                    f32::from(rgb[2]) / 255.0 * alpha,
                    alpha,
                ]
            }
        }
    }

    /// Bilinear sample at continuous image coordinates (pixel centres at .5).
    fn bilinear(&self, x: f64, y: f64) -> [f32; 4] {
        let clamp = |v: f64, len: usize| v.clamp(0.0, (len - 1) as f64);
        let (x, y) = (clamp(x - 0.5, self.width), clamp(y - 0.5, self.height));
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
        let (a, b) = (self.texel(x0, y0), self.texel(x1, y0));
        let (c, d) = (self.texel(x0, y1), self.texel(x1, y1));
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            top + (bottom - top) * fy
        })
    }

    /// Average `factor_x` x `factor_y` blocks (partial ones at the edges).
    fn reduced(&self, factor_x: usize, factor_y: usize) -> Source<'static> {
        let width = self.width.div_ceil(factor_x);
        let height = self.height.div_ceil(factor_y);
        let mut data = vec![[0f32; 4]; width * height];
        for y in 0..height {
            let rows = y * factor_y..((y + 1) * factor_y).min(self.height);
            for x in 0..width {
                let columns = x * factor_x..((x + 1) * factor_x).min(self.width);
                let mut sum = [0f32; 4];
                for sy in rows.clone() {
                    for sx in columns.clone() {
                        let texel = self.texel(sx, sy);
                        for c in 0..4 {
                            sum[c] += texel[c];
                        }
                    }
                }
                let count = (rows.len() * columns.len()) as f32;
                data[y * width + x] = sum.map(|c| c / count);
            }
        }
        Source {
            width,
            height,
            pixels: Pixels::Reduced(data),
        }
    }
}

/// One source resampled onto device pixels: for each device row, the image
/// rows it covers are blended into one row ([`AxisResampler::blend_row`]),
/// which each device column then reads ([`AxisResampler::sample`]).
struct AxisResampler<'s> {
    source: &'s Source<'s>,
    straight: bool,
    columns: Taps,
    rows: Taps,
    /// The stretch of the column axis any device column reads.
    low: usize,
    blended: Vec<[f32; 4]>,
}

impl<'s> AxisResampler<'s> {
    fn new(
        source: &'s Source<'s>,
        plan: &AxisPlan,
        width: usize,
        height: usize,
        interpolate: bool,
    ) -> Option<AxisResampler<'s>> {
        let columns = plan.columns.taps(plan.device_x(width), interpolate)?;
        let rows = plan.rows.taps(plan.device_y(height), interpolate)?;
        let (low, high) = columns.source_span();
        if low >= high {
            return None;
        }
        Some(AxisResampler {
            source,
            straight: plan.straight,
            columns,
            rows,
            low,
            blended: vec![[0.0; 4]; high - low],
        })
    }

    fn blend_row(&mut self, y: usize) {
        self.blended.fill([0.0; 4]);
        for &(along_rows, weight) in self.rows.at(y) {
            for (offset, slot) in self.blended.iter_mut().enumerate() {
                let along_columns = self.low + offset;
                let texel = if self.straight {
                    self.source.texel(along_columns, along_rows)
                } else {
                    self.source.texel(along_rows, along_columns)
                };
                for c in 0..4 {
                    slot[c] += weight * texel[c];
                }
            }
        }
    }

    fn sample(&self, x: usize) -> [f32; 4] {
        let mut sum = [0f32; 4];
        for &(along_columns, weight) in self.columns.at(x) {
            let texel = self.blended[along_columns - self.low];
            for c in 0..4 {
                sum[c] += weight * texel[c];
            }
        }
        sum
    }
}

/// An image whose axes run along the device's: each device column covers a
/// fixed run of one image axis, and each device row a run of the other.
struct AxisPlan {
    /// Image x runs along device x. False after a quarter turn, when image x
    /// runs along device y instead.
    straight: bool,
    columns: AxisMap,
    rows: AxisMap,
}

impl AxisPlan {
    /// The unit square maps to the device as `(a u + c v + e, b u + d v + f)`;
    /// image pixels sit at `(u W, (1 - v) H)`.
    fn new(ctm: &Matrix, width: usize, height: usize) -> Option<AxisPlan> {
        let (w, h) = (width as f64, height as f64);
        let Matrix { a, b, c, d, e, f } = *ctm;
        if ![a, b, c, d, e, f].iter().all(|v| v.is_finite()) {
            return None;
        }
        let tiny = |v: f64| v.abs() < AXIS_TOLERANCE;
        let usable = |v: f64| v.abs() > 1e-9;
        if tiny(b) && tiny(c) && usable(a) && usable(d) {
            Some(AxisPlan {
                straight: true,
                columns: AxisMap::new(w / a, -e * w / a, width, e, e + a),
                rows: AxisMap::new(-h / d, h + f * h / d, height, f, f + d),
            })
        } else if tiny(a) && tiny(d) && usable(b) && usable(c) {
            Some(AxisPlan {
                straight: false,
                columns: AxisMap::new(-h / c, h + e * h / c, height, e, e + c),
                rows: AxisMap::new(w / b, -f * w / b, width, f, f + b),
            })
        } else {
            None
        }
    }

    fn device_x(&self, width: usize) -> std::ops::Range<usize> {
        self.columns.device_range(width)
    }

    fn device_y(&self, height: usize) -> std::ops::Range<usize> {
        self.rows.device_range(height)
    }
}

/// Image coordinate `scale * device + offset` along one axis of `len` pixels,
/// which the image spans from device `from` to `to`.
struct AxisMap {
    scale: f64,
    offset: f64,
    len: usize,
    from: f64,
    to: f64,
}

impl AxisMap {
    fn new(scale: f64, offset: f64, len: usize, from: f64, to: f64) -> AxisMap {
        AxisMap {
            scale,
            offset,
            len,
            from: from.min(to),
            to: from.max(to),
        }
    }

    fn device_range(&self, limit: usize) -> std::ops::Range<usize> {
        let start = self.from.floor().clamp(0.0, limit as f64) as usize;
        let end = self.to.ceil().clamp(0.0, limit as f64) as usize;
        start..end.max(start)
    }

    /// The source pixels, and their weights, behind each device pixel in
    /// `range`. A device pixel whose centre falls outside the image gets
    /// none, so images meet their neighbours without a seam.
    fn taps(&self, range: std::ops::Range<usize>, interpolate: bool) -> Option<Taps> {
        let len = self.len as f64;
        let mut taps = Taps {
            first: range.start,
            spans: Vec::with_capacity(range.len()),
            entries: Vec::new(),
        };
        // Pathological scales would ask for billions of taps.
        if !(self.scale.abs() * range.len() as f64 <= 1e9) {
            return None;
        }
        for p in range {
            let start = taps.entries.len();
            let at = |device: f64| self.scale * device + self.offset;
            // Decided in device space, against the image's own extent: in
            // image space, rounding put a centre lying exactly on an edge
            // shared with a neighbour outside both images.
            if !(self.from..self.to).contains(&(p as f64 + 0.5)) {
                taps.spans.push(start..start);
                continue;
            }
            let centre = at(p as f64 + 0.5).clamp(0.0, len - 1e-9);
            let (low, high) = {
                let (a, b) = (at(p as f64), at(p as f64 + 1.0));
                (a.min(b), a.max(b))
            };
            if interpolate && high - low < 1.0 {
                // Enlarging, smoothly: a tent one source pixel wide.
                let t = (centre - 0.5).clamp(0.0, len - 1.0);
                let i0 = t.floor() as usize;
                let frac = (t - i0 as f64) as f32;
                taps.entries.push((i0, 1.0 - frac));
                if frac > 0.0 && i0 + 1 < self.len {
                    taps.entries.push((i0 + 1, frac));
                }
            } else {
                // Area: each source pixel weighs what it covers of this
                // device pixel's footprint. Enlarged, that is one pixel —
                // or two at a boundary, which keeps edges from stair-stepping.
                let (low, high) = (low.max(0.0), high.min(len));
                let first = low.floor() as usize;
                let last = (high.ceil() as usize).min(self.len);
                let mut total = 0.0f32;
                for i in first..last {
                    let overlap = (high.min(i as f64 + 1.0) - low.max(i as f64)) as f32;
                    if overlap > 0.0 {
                        taps.entries.push((i, overlap));
                        total += overlap;
                    }
                }
                if total > 0.0 {
                    for entry in &mut taps.entries[start..] {
                        entry.1 /= total;
                    }
                }
            }
            taps.spans.push(start..taps.entries.len());
        }
        Some(taps)
    }
}

/// Per device pixel, a run of `(source index, weight)` in `entries`.
struct Taps {
    first: usize,
    spans: Vec<std::ops::Range<usize>>,
    entries: Vec<(usize, f32)>,
}

impl Taps {
    /// Device pixels that read anything, with what they read.
    fn pixels(&self) -> impl Iterator<Item = (usize, &[(usize, f32)])> {
        self.spans
            .iter()
            .enumerate()
            .filter(|(_, span)| !span.is_empty())
            .map(|(i, span)| (self.first + i, &self.entries[span.clone()]))
    }

    /// What device pixel `p` reads; nothing outside the range or the image.
    fn at(&self, p: usize) -> &[(usize, f32)] {
        p.checked_sub(self.first)
            .and_then(|i| self.spans.get(i))
            .map_or(&[], |span| &self.entries[span.clone()])
    }

    /// The lowest and one past the highest source index any pixel reads.
    fn source_span(&self) -> (usize, usize) {
        let low = self.entries.iter().map(|e| e.0).min().unwrap_or(0);
        let high = self.entries.iter().map(|e| e.0 + 1).max().unwrap_or(0);
        (low, high)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{doc_with_content, pixel};
    use super::*;

    /// A page drawing one `width` x `height` gray image (row-major samples)
    /// into the rectangle given by `placement` (`a b c d e f` of a `cm`).
    fn page_with_image(
        samples: &[u8],
        width: usize,
        height: usize,
        placement: &str,
        extra: &str,
        media: [i64; 4],
    ) -> RenderedPage {
        let mut doc = doc_with_content(&format!("q {placement} cm /Im Do Q"), media);
        let dict = pdf_core::parser::Parser::new(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 {extra} >>"
            )
            .as_bytes(),
        )
        .parse_object()
        .unwrap();
        let image = doc.add_object(PdfObject::Stream(PdfStream::new(
            dict.as_dict().unwrap().clone(),
            samples.to_vec(),
        )));
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(
            "Resources".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "XObject".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "Im".into(),
                    PdfObject::Reference(image),
                )])),
            )])),
        );
        doc.set_object(id, PdfObject::Dictionary(page));
        render_page(&doc, 0, RenderOptions::default()).unwrap()
    }

    /// A page drawing a `width` x `height` grey image whose `/{mask_key}`
    /// is a second image described by `mask_dict`.
    #[allow(clippy::too_many_arguments)]
    fn page_with_masked_image(
        samples: &[u8],
        width: usize,
        height: usize,
        mask_key: &str,
        mask_dict: &str,
        mask_samples: &[u8],
        placement: &str,
        media: [i64; 4],
    ) -> RenderedPage {
        let mut doc = doc_with_content(&format!("q {placement} cm /Im Do Q"), media);
        let parse = |source: String| {
            pdf_core::parser::Parser::new(source.as_bytes())
                .parse_object()
                .unwrap()
                .as_dict()
                .unwrap()
                .clone()
        };
        let mask = doc.add_object(PdfObject::Stream(PdfStream::new(
            parse(format!("<< /Type /XObject /Subtype /Image {mask_dict} >>")),
            mask_samples.to_vec(),
        )));
        let image = doc.add_object(PdfObject::Stream(PdfStream::new(
            parse(format!(
                "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} \
                 /ColorSpace /DeviceGray /BitsPerComponent 8 /{mask_key} {} {} R >>",
                mask.number, mask.generation
            )),
            samples.to_vec(),
        )));
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(
            "Resources".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "XObject".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "Im".into(),
                    PdfObject::Reference(image),
                )])),
            )])),
        );
        doc.set_object(id, PdfObject::Dictionary(page));
        render_page(&doc, 0, RenderOptions::default()).unwrap()
    }

    /// The layout of a scanned book from the Internet Archive: a dark layer
    /// shown only through a text mask. Painting the whole layer, as ignoring
    /// /Mask did, greyed out the page.
    #[test]
    fn an_explicit_mask_shows_the_image_only_where_it_paints() {
        // 8x1 mask, one bit per pixel: 0 (painted) on the left half.
        let page = page_with_masked_image(
            &[0; 8],
            8,
            1,
            "Mask",
            "/Width 8 /Height 1 /ImageMask true",
            &[0b0000_1111],
            "8 0 0 2 0 0",
            [0, 0, 8, 2],
        );
        assert_eq!(pixel(&page, 1, 1), (0, 0, 0));
        assert_eq!(pixel(&page, 6, 1), (255, 255, 255));
    }

    #[test]
    fn a_finer_mask_keeps_its_own_resolution() {
        // A single black pixel of colour under a 16-pixel mask: the mask
        // decides each of the 16 device pixels, not the image's one.
        let page = page_with_masked_image(
            &[0],
            1,
            1,
            "Mask",
            "/Width 16 /Height 1 /ImageMask true",
            &[0b0101_0101, 0b0101_0101],
            "16 0 0 2 0 0",
            [0, 0, 16, 2],
        );
        for x in 0..16 {
            let expected = if x % 2 == 0 {
                (0, 0, 0)
            } else {
                (255, 255, 255)
            };
            assert_eq!(pixel(&page, x, 1), expected, "column {x}");
        }
    }

    #[test]
    fn a_colour_key_makes_matching_samples_transparent() {
        // Samples 0 and 10 fall in the key range [0 20]; 200 does not.
        let page = page_with_image(
            &[0, 10, 200],
            3,
            1,
            "3 0 0 1 0 0",
            "/Mask [0 20]",
            [0, 0, 3, 1],
        );
        assert_eq!(pixel(&page, 0, 0), (255, 255, 255));
        assert_eq!(pixel(&page, 1, 0), (255, 255, 255));
        assert_eq!(pixel(&page, 2, 0), (200, 200, 200));
    }

    #[test]
    fn a_soft_mask_of_another_size_keeps_its_own_resolution() {
        // Squeezed onto the one-pixel image, as soft masks used to be, the
        // mask's first sample would have made the whole image opaque.
        let page = page_with_masked_image(
            &[0],
            1,
            1,
            "SMask",
            "/Width 2 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
            &[255, 0],
            "2 0 0 2 0 0",
            [0, 0, 2, 2],
        );
        assert_eq!(pixel(&page, 0, 1), (0, 0, 0));
        assert_eq!(pixel(&page, 1, 1), (255, 255, 255));
    }

    #[test]
    fn a_turned_image_honours_its_mask() {
        // The same half-painted mask, turned 30 degrees: one half dark, the
        // other the white page.
        let (cos, sin) = (
            30f64.to_radians().cos() * 20.0,
            30f64.to_radians().sin() * 20.0,
        );
        let page = page_with_masked_image(
            &[0; 64],
            8,
            8,
            "Mask",
            "/Width 2 /Height 1 /ImageMask true",
            &[0b0100_0000],
            &format!("{cos} {sin} {} {cos} 20 5", -sin),
            [0, 0, 40, 40],
        );
        let dark = page.pixels.chunks_exact(4).filter(|p| p[0] < 40).count();
        let white = page.pixels.chunks_exact(4).filter(|p| p[0] > 215).count();
        // The image covers 400 pixels, half of them shown.
        assert!((150..=250).contains(&dark), "{dark} dark pixels");
        assert!(white > 1600 - 250, "{white} white pixels");
    }

    /// One-pixel black and white stripes, 40 wide: shrunk to 10 device
    /// pixels, every device pixel is half of each — even grey — where
    /// picking single source pixels gave solid black or solid white.
    #[test]
    fn shrunk_images_average_the_area_they_cover() {
        let stripes: Vec<u8> = (0..40 * 4)
            .map(|i| if i % 2 == 0 { 0 } else { 255 })
            .collect();
        let page = page_with_image(&stripes, 40, 4, "10 0 0 4 0 0", "", [0, 0, 10, 4]);
        for x in 0..10 {
            let (r, _, _) = pixel(&page, x, 2);
            assert!((r as i32 - 128).abs() <= 3, "column {x}: {r}");
        }
    }

    #[test]
    fn a_quarter_turn_averages_too() {
        // The same stripes rotated 90 degrees: they now run down the page.
        let stripes: Vec<u8> = (0..40 * 4)
            .map(|i| if i % 2 == 0 { 0 } else { 255 })
            .collect();
        let page = page_with_image(&stripes, 40, 4, "0 10 -4 0 4 0", "", [0, 0, 4, 10]);
        for y in 0..10 {
            let (r, _, _) = pixel(&page, 2, y);
            assert!((r as i32 - 128).abs() <= 3, "row {y}: {r}");
        }
    }

    #[test]
    fn enlarged_images_keep_crisp_pixels_unless_interpolated() {
        // Two pixels, black then white, drawn 20 device pixels wide.
        let page = page_with_image(&[0, 255], 2, 1, "20 0 0 4 0 0", "", [0, 0, 20, 4]);
        assert_eq!(pixel(&page, 4, 2), (0, 0, 0));
        assert_eq!(pixel(&page, 15, 2), (255, 255, 255));
        let smooth = page_with_image(
            &[0, 255],
            2,
            1,
            "20 0 0 4 0 0",
            "/Interpolate true",
            [0, 0, 20, 4],
        );
        let (r, _, _) = pixel(&smooth, 8, 2);
        assert!((40..215).contains(&r), "interpolated ramp, got {r}");
    }

    #[test]
    fn adjacent_images_meet_without_a_seam() {
        // Two black images sharing an edge at a fractional device position.
        let mut doc = doc_with_content(
            "q 5.5 0 0 4 0 0 cm /Im Do Q q 4.5 0 0 4 5.5 0 cm /Im Do Q",
            [0, 0, 10, 4],
        );
        let dict = pdf_core::parser::Parser::new(
            b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 >>",
        )
        .parse_object()
        .unwrap();
        let image = doc.add_object(PdfObject::Stream(PdfStream::new(
            dict.as_dict().unwrap().clone(),
            vec![0],
        )));
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(
            "Resources".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "XObject".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "Im".into(),
                    PdfObject::Reference(image),
                )])),
            )])),
        );
        doc.set_object(id, PdfObject::Dictionary(page));
        let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
        for x in 0..10 {
            assert_eq!(pixel(&page, x, 2), (0, 0, 0), "column {x}");
        }
    }

    #[test]
    fn turned_images_are_smoothed_and_keep_their_tone() {
        // A uniform mid-grey image turned 30 degrees: its inside stays that
        // grey, and its edges are partial rather than jagged.
        let gray = vec![100u8; 16 * 16];
        let (cos, sin) = (
            30f64.to_radians().cos() * 20.0,
            30f64.to_radians().sin() * 20.0,
        );
        let page = page_with_image(
            &gray,
            16,
            16,
            &format!("{cos} {sin} {} {cos} 20 5", -sin),
            "",
            [0, 0, 40, 40],
        );
        let (r, _, _) = pixel(&page, 20, 20);
        assert!((r as i32 - 100).abs() <= 2, "inside {r}");
        let partial = page
            .pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 110 && p[0] < 245)
            .count();
        assert!(partial > 10, "edges should blend, {partial} partial pixels");
    }
}
