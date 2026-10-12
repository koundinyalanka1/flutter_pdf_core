//! Scanline rasterizer and RGBA canvas.
//!
//! Anti-aliasing is 16 sub-scanlines per pixel row with exact horizontal span
//! coverage. Winding is counted exactly on every sub-scanline, so overlapping
//! pieces of one path (stroke segments, joins) union without darkened seams,
//! and the inner loop stays simple f32 accumulation. Only edges crossing the
//! current row are visited, and only the pixels a row touched are written.

use std::rc::Rc;

#[path = "blend.rs"]
mod blending;
pub use blending::lum as luminosity;
pub use blending::BlendMode;

use crate::geom::{FillRule, Path, Point};

/// Sub-scanlines sampled per pixel row. Coverage across a nearly horizontal
/// edge comes in steps of 1/16, too fine to see; with 4, hairline horizontal
/// strokes (CJK, a Devanagari headline at body size) could fall between the
/// samples and drop out, and the tops of letters came in visible steps.
const SUB_SAMPLES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    };

    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    pub fn gray(v: f32) -> Self {
        Self::new(v, v, v)
    }

    pub fn from_cmyk(c: f32, m: f32, y: f32, k: f32) -> Self {
        Self::new(
            (1.0 - c) * (1.0 - k),
            (1.0 - m) * (1.0 - k),
            (1.0 - y) * (1.0 - k),
        )
    }
}

/// Supported transparency blending spaces. Gray samples use equal RGB channels
/// in the output buffer, so a completed gray group can be painted into RGB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlendColorSpace {
    #[default]
    DeviceRgb,
    DeviceGray,
}

impl BlendColorSpace {
    fn convert(self, color: [f32; 3]) -> [f32; 3] {
        match self {
            Self::DeviceRgb => color,
            // ISO 32000-1, 10.3.2: convert before applying the blend function,
            // rather than desaturating an already-composited RGB group.
            Self::DeviceGray => [luminosity(color); 3],
        }
    }
}

/// An 8-bit coverage mask the size of the canvas. Used for clipping.
#[derive(Debug, Clone)]
pub struct Mask {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn opaque(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![255; width * height],
        }
    }

    /// Intersection of two masks (multiply).
    pub fn intersect(&self, other: &Mask) -> Mask {
        let data = self
            .data
            .iter()
            .zip(other.data.iter())
            .map(|(&a, &b)| ((a as u16 * b as u16) / 255) as u8)
            .collect();
        Mask {
            width: self.width,
            height: self.height,
            data,
        }
    }
}

/// Straight RGBA8 output. The page is opaque; intermediate groups can be transparent.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// Straight RGBA, row-major, top-left origin.
    pub pixels: Vec<u8>,
    /// Reusable coverage row so filling does not allocate per scanline.
    coverage: Vec<f32>,
    crossings: Vec<(f64, i32)>,
    pub blend_mode: BlendMode,
    pub blend_color_space: BlendColorSpace,
    pub paint_suppressed: bool,
    pub soft_mask: ClipMask,
    pub alpha_is_shape: bool,
    /// Set while glyph outlines are filled: see [`glyph_coverage`].
    pub glyph_fill: bool,
    pub group_alpha: Option<Vec<f32>>,
    pub group_shape: Option<Vec<f32>>,
    knockout_backdrop: Option<Vec<u8>>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![255; width * height * 4],
            coverage: vec![0.0; width + 2],
            crossings: Vec::with_capacity(64),
            blend_mode: BlendMode::Normal,
            blend_color_space: BlendColorSpace::DeviceRgb,
            paint_suppressed: false,
            soft_mask: None,
            alpha_is_shape: false,
            glyph_fill: false,
            group_alpha: None,
            group_shape: None,
            knockout_backdrop: None,
        }
    }

    pub fn transparent(width: usize, height: usize) -> Self {
        let mut canvas = Self::new(width, height);
        canvas.pixels.fill(0);
        canvas
    }

    pub fn group(backdrop: &Canvas, isolated: bool, knockout: bool) -> Self {
        Self::group_in_color_space(backdrop, isolated, knockout, backdrop.blend_color_space)
    }

    pub fn group_in_color_space(
        backdrop: &Canvas,
        isolated: bool,
        knockout: bool,
        blend_color_space: BlendColorSpace,
    ) -> Self {
        let mut canvas = Self::transparent(backdrop.width, backdrop.height);
        canvas.blend_color_space = blend_color_space;
        canvas.paint_suppressed = backdrop.paint_suppressed;
        if !isolated {
            canvas.pixels.copy_from_slice(backdrop.group_backdrop());
            if blend_color_space != backdrop.blend_color_space {
                for pixel in canvas.pixels.chunks_exact_mut(4) {
                    let color = blend_color_space.convert([
                        pixel[0] as f32 / 255.0,
                        pixel[1] as f32 / 255.0,
                        pixel[2] as f32 / 255.0,
                    ]);
                    for (sample, value) in pixel[..3].iter_mut().zip(color) {
                        *sample = to_u8(value);
                    }
                }
            }
        }
        if knockout {
            canvas.knockout_backdrop = Some(canvas.pixels.clone());
        }
        canvas.group_alpha = Some(vec![0.0; backdrop.width * backdrop.height]);
        canvas.group_shape = Some(vec![0.0; backdrop.width * backdrop.height]);
        canvas
    }

    /// A non-isolated child of a knockout group inherits the parent's initial
    /// backdrop, not the objects already painted into that parent (11.4.6).
    pub fn group_backdrop(&self) -> &[u8] {
        self.knockout_backdrop.as_deref().unwrap_or(&self.pixels)
    }

    pub fn fill_background(&mut self, color: Rgb) {
        let [r, g, b] = self
            .blend_color_space
            .convert([color.r, color.g, color.b])
            .map(to_u8);
        for pixel in self.pixels.chunks_exact_mut(4) {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
            pixel[3] = 255;
        }
    }

    /// Fill `path` (device space) with a flat colour.
    pub fn fill_path(
        &mut self,
        path: &Path,
        color: Rgb,
        rule: FillRule,
        alpha: f32,
        clip: Option<&Mask>,
    ) {
        if self.paint_suppressed || (alpha <= 0.001 && self.knockout_backdrop.is_none()) {
            return;
        }
        self.scan(path, rule, |canvas, y, row, span| {
            canvas.blend_row(y, row, span, color, alpha, clip);
        });
    }

    /// Rasterize `path` into a standalone coverage mask (for `W n` clipping).
    pub fn rasterize_mask(&mut self, path: &Path, rule: FillRule) -> Mask {
        let mut mask = Mask {
            width: self.width,
            height: self.height,
            data: vec![0; self.width * self.height],
        };
        let width = self.width;
        self.scan(path, rule, |_canvas, y, row, span| {
            let base = y * width;
            for x in span {
                let c = row[x].clamp(0.0, 1.0);
                if c > 0.0 {
                    mask.data[base + x] = (c * 255.0 + 0.5) as u8;
                }
            }
        });
        mask
    }

    /// Shared scanline walk. Calls `emit(self, y, coverage_row, span)` per
    /// pixel row that the path touches; only `span` of the row holds coverage.
    fn scan<F>(&mut self, path: &Path, rule: FillRule, mut emit: F)
    where
        F: FnMut(&mut Canvas, usize, &[f32], std::ops::Range<usize>),
    {
        let mut edges = collect_edges(path);
        if edges.is_empty() {
            return;
        }

        let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
        for edge in &edges {
            min_y = min_y.min(edge.y_min);
            max_y = max_y.max(edge.y_max);
        }
        let y_start = (min_y.floor().max(0.0)) as usize;
        let y_end = (max_y.ceil().min(self.height as f64)) as usize;
        if y_start >= y_end {
            return;
        }
        // Edges join the active list as the walk reaches their top, so each
        // row only looks at the few edges that actually cross it.
        edges.sort_unstable_by(|a, b| a.y_min.total_cmp(&b.y_min));
        let mut active: Vec<usize> = Vec::new();
        let mut next = 0;

        let width = self.width;
        let sub_weight = 1.0 / SUB_SAMPLES as f32;

        for y in y_start..y_end {
            let top = y as f64;
            while next < edges.len() && edges[next].y_min < top + 1.0 {
                active.push(next);
                next += 1;
            }
            active.retain(|&index| edges[index].y_max > top);
            if active.is_empty() {
                if next == edges.len() {
                    break;
                }
                continue;
            }
            let mut touched: Option<(usize, usize)> = None;

            for sub in 0..SUB_SAMPLES {
                let sample_y = top + (sub as f64 + 0.5) / SUB_SAMPLES as f64;
                self.crossings.clear();
                for &index in &active {
                    let edge = &edges[index];
                    if sample_y >= edge.y_min && sample_y < edge.y_max {
                        self.crossings
                            .push((edge.x0 + (sample_y - edge.y0) * edge.dxdy, edge.winding));
                    }
                }
                if self.crossings.len() < 2 {
                    continue;
                }
                self.crossings.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));

                let mut winding = 0;
                for pair in 0..self.crossings.len() - 1 {
                    winding += self.crossings[pair].1;
                    let inside = match rule {
                        FillRule::NonZero => winding != 0,
                        FillRule::EvenOdd => winding % 2 != 0,
                    };
                    if !inside {
                        continue;
                    }
                    let x_from = self.crossings[pair].0;
                    let x_to = self.crossings[pair + 1].0;
                    if let Some((first, last)) =
                        add_span(&mut self.coverage, width, x_from, x_to, sub_weight)
                    {
                        touched = Some(match touched {
                            Some((lo, hi)) => (lo.min(first), hi.max(last)),
                            None => (first, last),
                        });
                    }
                }
            }

            if let Some((lo, hi)) = touched {
                // Split the borrow: `emit` needs &mut Canvas while reading the row.
                let row = std::mem::take(&mut self.coverage);
                emit(self, y, &row, lo..hi);
                self.coverage = row;
                // Only this span was written, so only it needs clearing.
                self.coverage[lo..hi].fill(0.0);
            }
        }
    }

    fn blend_row(
        &mut self,
        y: usize,
        coverage: &[f32],
        span: std::ops::Range<usize>,
        color: Rgb,
        alpha: f32,
        clip: Option<&Mask>,
    ) {
        let r = color.r.clamp(0.0, 1.0);
        let g = color.g.clamp(0.0, 1.0);
        let b = color.b.clamp(0.0, 1.0);
        let row_base = y * self.width;
        for x in span {
            let mut shape = coverage[x].clamp(0.0, 1.0);
            if shape <= 0.002 {
                continue;
            }
            if self.glyph_fill {
                shape = glyph_coverage(shape);
            }
            if let Some(mask) = clip {
                shape *= mask.data[row_base + x] as f32 / 255.0;
                if shape <= 0.002 {
                    continue;
                }
            }
            self.blend_at(row_base + x, [r, g, b], shape * alpha, shape);
        }
    }

    /// Blend a single device pixel — used by the image painter.
    pub fn blend(&mut self, x: usize, y: usize, color: Rgb, alpha: f32, clip: Option<&Mask>) {
        if x >= self.width
            || y >= self.height
            || (alpha <= 0.002 && self.knockout_backdrop.is_none())
        {
            return;
        }
        let index = y * self.width + x;
        let mut shape = 1.0;
        if let Some(mask) = clip {
            shape *= mask.data[index] as f32 / 255.0;
            if shape <= 0.002 {
                return;
            }
        }
        self.blend_at(
            index,
            [
                color.r.clamp(0.0, 1.0),
                color.g.clamp(0.0, 1.0),
                color.b.clamp(0.0, 1.0),
            ],
            alpha * shape,
            shape,
        );
    }

    pub(super) fn blend_at(
        &mut self,
        index: usize,
        source: [f32; 3],
        mut alpha: f32,
        mut shape: f32,
    ) {
        if self.paint_suppressed {
            return;
        }
        let source = self.blend_color_space.convert(source);
        if let Some(mask) = &self.soft_mask {
            alpha *= mask.data[index] as f32 / 255.0;
        }
        alpha = alpha.clamp(0.0, 1.0);
        if self.alpha_is_shape {
            shape = alpha;
        }
        let offset = index * 4;
        if let Some(backdrop) = &self.knockout_backdrop {
            if shape <= 0.0 {
                return;
            }
            let mut painted: [u8; 4] = backdrop[offset..offset + 4].try_into().unwrap();
            blending::composite(&mut painted, source, alpha / shape, self.blend_mode);
            let previous = &mut self.pixels[offset..offset + 4];
            let a_old = previous[3] as f32 / 255.0;
            let a_new = painted[3] as f32 / 255.0;
            let a = (1.0 - shape) * a_old + shape * a_new;
            for c in 0..3 {
                previous[c] = if a > 0.0 {
                    (((1.0 - shape) * a_old * previous[c] as f32
                        + shape * a_new * painted[c] as f32)
                        / a
                        + 0.5) as u8
                } else {
                    0
                };
            }
            previous[3] = to_u8(a);
        } else {
            blending::composite(
                &mut self.pixels[offset..offset + 4],
                source,
                alpha,
                self.blend_mode,
            );
        }
        if let Some(group_alpha) = &mut self.group_alpha {
            let factor = if self.knockout_backdrop.is_some() {
                shape
            } else {
                alpha
            };
            group_alpha[index] = alpha + (1.0 - factor) * group_alpha[index];
        }
        if let Some(group_shape) = &mut self.group_shape {
            group_shape[index] = shape + (1.0 - shape) * group_shape[index];
        }
    }
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Coverage for the edge pixels of a glyph outline.
///
/// Exact, unhinted outlines come out a little lighter at reading sizes than
/// the same text in PDFium, whose FreeType hinting pulls stems onto whole
/// pixels — measured against it at phone resolution, 4–6% less ink across
/// Latin, Indic and CJK fonts. Raising partial coverage to the power 0.9
/// restores the same weight (within 1%). Solid pixels are unchanged, so large
/// text, which has few partial pixels, keeps its exact shape.
fn glyph_coverage(coverage: f32) -> f32 {
    static TABLE: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| std::array::from_fn(|i| (i as f32 / 255.0).powf(0.9)));
    table[(coverage.clamp(0.0, 1.0) * 255.0 + 0.5) as usize]
}

/// Accumulate horizontal coverage for `[x_from, x_to)` on one sub-scanline.
/// Returns the pixels written, as `first..last`, when any landed inside the
/// canvas.
fn add_span(
    coverage: &mut [f32],
    width: usize,
    x_from: f64,
    x_to: f64,
    weight: f32,
) -> Option<(usize, usize)> {
    let left = x_from.max(0.0);
    let right = x_to.min(width as f64);
    if right <= left {
        return None;
    }
    let first = left.floor() as usize;
    let last = (right.ceil() as usize).min(width);
    if first >= width {
        return None;
    }

    if last - first == 1 {
        // Span sits inside a single pixel.
        coverage[first] += weight * (right - left) as f32;
        return Some((first, last));
    }
    // Partial first pixel, solid middle, partial last pixel.
    coverage[first] += weight * ((first + 1) as f64 - left) as f32;
    for x in (first + 1)..last.saturating_sub(1) {
        coverage[x] += weight;
    }
    if last >= 1 && last - 1 > first {
        coverage[last - 1] += weight * (right - (last - 1) as f64) as f32;
    }
    Some((first, last))
}

struct Edge {
    x0: f64,
    y0: f64,
    /// Change in x per unit of y, so a crossing is one multiply-add.
    dxdy: f64,
    y_min: f64,
    y_max: f64,
    winding: i32,
}

/// Turn the flattened path into non-horizontal directed edges. Open subpaths
/// are implicitly closed, which is what PDF fill operators do.
fn collect_edges(path: &Path) -> Vec<Edge> {
    let mut edges = Vec::new();
    for subpath in &path.subpaths {
        let points = &subpath.points;
        if points.len() < 2 {
            continue;
        }
        for i in 0..points.len() {
            let start = points[i];
            let end = if i + 1 < points.len() {
                points[i + 1]
            } else {
                points[0]
            };
            push_edge(&mut edges, start, end);
        }
    }
    edges
}

fn push_edge(edges: &mut Vec<Edge>, start: Point, end: Point) {
    if (start.y - end.y).abs() < 1e-12 {
        return; // Horizontal edges contribute no crossings.
    }
    // A degenerate transform can produce NaN or infinite points; such an
    // edge has no sensible crossing and would only stall the walk.
    if ![start.x, start.y, end.x, end.y]
        .iter()
        .all(|v| v.is_finite())
    {
        return;
    }
    let winding = if end.y > start.y { 1 } else { -1 };
    edges.push(Edge {
        x0: start.x,
        y0: start.y,
        dxdy: (end.x - start.x) / (end.y - start.y),
        y_min: start.y.min(end.y),
        y_max: start.y.max(end.y),
        winding,
    });
}

/// Convert a path into a fillable stroke, with PDF cap/join styles.
pub fn stroke_outline(path: &Path, width: f64) -> Path {
    stroke_outline_styled(path, width, 1, 1, 10.0)
}

pub fn stroke_outline_with_cap(path: &Path, width: f64, cap: i64) -> Path {
    stroke_outline_styled(path, width, cap, 1, 10.0)
}

pub fn stroke_outline_styled(
    path: &Path,
    width: f64,
    cap: i64,
    join: i64,
    miter_limit: f64,
) -> Path {
    let half = width.abs() / 2.0;
    let mut out = Path::new();
    if !half.is_finite() || half <= 0.0 {
        return out;
    }
    for subpath in &path.subpaths {
        let mut points = subpath.points.clone();
        points.dedup();
        if subpath.closed && points.len() > 1 && points.first() == points.last() {
            points.pop();
        }
        if points.len() == 1 {
            if cap == 1 {
                push_disc(&mut out, points[0], half);
            }
            continue;
        }
        let count = points.len();
        if count < 2 {
            continue;
        }
        let segments = if subpath.closed { count } else { count - 1 };
        for i in 0..segments {
            let (mut a, mut b) = (points[i], points[(i + 1) % count]);
            let length = (b.x - a.x).hypot(b.y - a.y);
            if length < 1e-12 {
                continue;
            }
            let (dx, dy) = ((b.x - a.x) / length, (b.y - a.y) / length);
            let (nx, ny) = (-dy * half, dx * half);
            if cap == 2 && !subpath.closed {
                if i == 0 {
                    a.x -= dx * half;
                    a.y -= dy * half;
                }
                if i + 1 == segments {
                    b.x += dx * half;
                    b.y += dy * half;
                }
            }
            polygon(
                &mut out,
                &[
                    Point::new(a.x + nx, a.y + ny),
                    Point::new(b.x + nx, b.y + ny),
                    Point::new(b.x - nx, b.y - ny),
                    Point::new(a.x - nx, a.y - ny),
                ],
            );
        }
        if !subpath.closed && cap == 1 {
            push_disc(&mut out, points[0], half);
            push_disc(&mut out, points[count - 1], half);
        }
        let joins = if subpath.closed {
            0..count
        } else {
            1..count - 1
        };
        for i in joins {
            let (a, b, c) = (
                points[(i + count - 1) % count],
                points[i],
                points[(i + 1) % count],
            );
            let l1 = (b.x - a.x).hypot(b.y - a.y);
            let l2 = (c.x - b.x).hypot(c.y - b.y);
            if l1 < 1e-12 || l2 < 1e-12 {
                continue;
            }
            let (u, v) = (
                Point::new((b.x - a.x) / l1, (b.y - a.y) / l1),
                Point::new((c.x - b.x) / l2, (c.y - b.y) / l2),
            );
            let cross = u.x * v.y - u.y * v.x;
            if join == 1 {
                push_disc(&mut out, b, half);
                continue;
            }
            if cross.abs() < 1e-12 {
                continue;
            }
            let side = -cross.signum();
            let p = Point::new(b.x - u.y * half * side, b.y + u.x * half * side);
            let q = Point::new(b.x - v.y * half * side, b.y + v.x * half * side);
            let t = ((q.x - p.x) * v.y - (q.y - p.y) * v.x) / cross;
            let tip = Point::new(p.x + t * u.x, p.y + t * u.y);
            if join == 0 && (tip.x - b.x).hypot(tip.y - b.y) <= half * miter_limit.max(1.0) {
                polygon(&mut out, &[b, p, tip, q]);
            } else {
                polygon(&mut out, &[b, p, q]);
            }
        }
    }
    out
}

// All component polygons have the same winding so overlaps form a union.
fn polygon(path: &mut Path, points: &[Point]) {
    if points.len() < 3 {
        return;
    }
    let area: f64 = points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| a.x * b.y - b.x * a.y)
        .sum();
    path.move_to(points[0].x, points[0].y);
    if area > 0.0 {
        for point in points[1..].iter().rev() {
            path.line_to(point.x, point.y);
        }
    } else {
        for point in &points[1..] {
            path.line_to(point.x, point.y);
        }
    }
    path.close();
}

fn push_disc(path: &mut Path, center: Point, radius: f64) {
    let segments = ((radius * std::f64::consts::TAU / 0.5).ceil() as usize).clamp(12, 128);
    for i in 0..segments {
        let angle = -(i as f64) / segments as f64 * std::f64::consts::TAU;
        let (x, y) = (
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        );
        if i == 0 {
            path.move_to(x, y);
        } else {
            path.line_to(x, y);
        }
    }
    path.close();
}

/// Shared clip state. `None` means "no clipping". Reference counted because
/// `q`/`Q` copies the graphics state constantly and masks are page-sized.
pub type ClipMask = Option<Rc<Mask>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Matrix;

    fn pixel(canvas: &Canvas, x: usize, y: usize) -> (u8, u8, u8) {
        let offset = (y * canvas.width + x) * 4;
        (
            canvas.pixels[offset],
            canvas.pixels[offset + 1],
            canvas.pixels[offset + 2],
        )
    }

    #[test]
    fn fills_a_rectangle_opaquely() {
        let mut canvas = Canvas::new(20, 20);
        let mut path = Path::new();
        path.rect(5.0, 5.0, 10.0, 10.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);

        assert_eq!(pixel(&canvas, 10, 10), (0, 0, 0), "inside should be black");
        assert_eq!(pixel(&canvas, 1, 1), (255, 255, 255), "outside stays white");
    }

    #[test]
    fn even_odd_leaves_a_hole() {
        let mut canvas = Canvas::new(40, 40);
        let mut path = Path::new();
        path.rect(5.0, 5.0, 30.0, 30.0);
        path.rect(15.0, 15.0, 10.0, 10.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::EvenOdd, 1.0, None);

        assert_eq!(pixel(&canvas, 10, 20), (0, 0, 0), "ring is filled");
        assert_eq!(pixel(&canvas, 20, 20), (255, 255, 255), "centre is a hole");
    }

    #[test]
    fn non_zero_fills_the_hole_when_windings_agree() {
        let mut canvas = Canvas::new(40, 40);
        let mut path = Path::new();
        path.rect(5.0, 5.0, 30.0, 30.0);
        path.rect(15.0, 15.0, 10.0, 10.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        assert_eq!(pixel(&canvas, 20, 20), (0, 0, 0));
    }

    #[test]
    fn edges_are_anti_aliased() {
        let mut canvas = Canvas::new(20, 20);
        let mut path = Path::new();
        path.rect(5.0, 5.5, 10.0, 9.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        let (r, _, _) = pixel(&canvas, 10, 5);
        assert!(
            r > 0 && r < 255,
            "half-covered pixel should be grey, got {r}"
        );
    }

    #[test]
    fn hairline_horizontal_shapes_do_not_drop_out() {
        // A tenth of a pixel tall, between where four sub-scanlines sampled
        // (y + 0.125 and y + 0.375).
        let mut canvas = Canvas::new(20, 20);
        let mut path = Path::new();
        path.move_to(2.0, 5.14);
        path.line_to(18.0, 5.15);
        path.line_to(18.0, 5.25);
        path.line_to(2.0, 5.24);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        let (r, _, _) = pixel(&canvas, 10, 5);
        assert!(r < 255, "hairline vanished");
    }

    #[test]
    fn horizontal_edge_coverage_is_accurate() {
        let mut canvas = Canvas::new(20, 20);
        let mut path = Path::new();
        path.rect(2.0, 5.3, 16.0, 10.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        // Row 5 is 70% covered: 255 * 0.3 = 76.5, within a sixteenth.
        let (r, _, _) = pixel(&canvas, 10, 5);
        assert!((r as f32 - 76.5).abs() <= 255.0 / 32.0 + 1.0, "got {r}");
    }

    #[test]
    fn glyph_edges_are_slightly_heavier_and_solid_pixels_unchanged() {
        assert_eq!(glyph_coverage(1.0), 1.0);
        assert_eq!(glyph_coverage(0.0), 0.0);
        let half = glyph_coverage(0.5);
        assert!(
            half > 0.5 && (half - 0.5f32.powf(0.9)).abs() < 0.005,
            "{half}"
        );

        let mut path = Path::new();
        path.rect(2.0, 2.3, 10.0, 5.0); // row 2 is 70% covered
        let mut plain = Canvas::new(16, 10);
        plain.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        let mut glyph = Canvas::new(16, 10);
        glyph.glyph_fill = true;
        glyph.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        assert!(pixel(&glyph, 5, 2).0 < pixel(&plain, 5, 2).0);
        assert_eq!(pixel(&glyph, 5, 4), pixel(&plain, 5, 4));
    }

    #[test]
    fn rows_outside_a_shape_are_untouched_and_coverage_does_not_leak() {
        let mut canvas = Canvas::new(40, 10);
        let mut first = Path::new();
        first.rect(30.0, 2.0, 5.0, 5.0);
        canvas.fill_path(&first, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        let mut second = Path::new();
        second.rect(2.0, 2.0, 5.0, 5.0);
        canvas.fill_path(&second, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        // The first fill's coverage must not linger in the reused row buffer.
        assert_eq!(pixel(&canvas, 20, 4), (255, 255, 255));
        assert_eq!(pixel(&canvas, 32, 4), (0, 0, 0));
        assert_eq!(pixel(&canvas, 4, 4), (0, 0, 0));
    }

    #[test]
    fn clip_mask_limits_the_fill() {
        let mut canvas = Canvas::new(20, 20);
        let mut clip_path = Path::new();
        clip_path.rect(0.0, 0.0, 10.0, 20.0);
        let clip = canvas.rasterize_mask(&clip_path, FillRule::NonZero);

        let mut path = Path::new();
        path.rect(0.0, 0.0, 20.0, 20.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, Some(&clip));

        assert_eq!(pixel(&canvas, 5, 10), (0, 0, 0), "inside clip");
        assert_eq!(pixel(&canvas, 15, 10), (255, 255, 255), "outside clip");
    }

    #[test]
    fn alpha_blends_towards_the_background() {
        let mut canvas = Canvas::new(10, 10);
        let mut path = Path::new();
        path.rect(0.0, 0.0, 10.0, 10.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 0.5, None);
        let (r, _, _) = pixel(&canvas, 5, 5);
        assert!((r as i32 - 128).abs() <= 2, "expected ~50% grey, got {r}");
    }

    #[test]
    fn stroke_outline_covers_the_line() {
        let mut canvas = Canvas::new(20, 20);
        let mut line = Path::new();
        line.move_to(2.0, 10.0);
        line.line_to(18.0, 10.0);
        let outline = stroke_outline(&line, 4.0);
        canvas.fill_path(&outline, Rgb::BLACK, FillRule::NonZero, 1.0, None);

        assert_eq!(pixel(&canvas, 10, 10), (0, 0, 0));
        assert_eq!(pixel(&canvas, 10, 2), (255, 255, 255));
    }

    #[test]
    fn transformed_paths_land_where_expected() {
        let mut canvas = Canvas::new(40, 40);
        let mut path = Path::new();
        path.rect(0.0, 0.0, 10.0, 10.0);
        let moved = path.transform(&Matrix::translate(20.0, 20.0));
        canvas.fill_path(&moved, Rgb::BLACK, FillRule::NonZero, 1.0, None);

        assert_eq!(pixel(&canvas, 25, 25), (0, 0, 0));
        assert_eq!(pixel(&canvas, 5, 5), (255, 255, 255));
    }

    #[test]
    fn cmyk_black_converts_to_black() {
        let rgb = Rgb::from_cmyk(0.0, 0.0, 0.0, 1.0);
        assert_eq!((rgb.r, rgb.g, rgb.b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn gray_group_converts_each_source_before_multiplying() {
        let parent = Canvas::new(1, 1);
        let mut gray =
            Canvas::group_in_color_space(&parent, true, false, BlendColorSpace::DeviceGray);
        gray.blend(0, 0, Rgb::new(1.0, 0.0, 0.0), 1.0, None);
        gray.blend_mode = BlendMode::Multiply;
        gray.blend(0, 0, Rgb::new(0.0, 1.0, 0.0), 1.0, None);
        // Gray(red) * Gray(green) = .30 * .59. Multiplying in RGB first
        // would incorrectly produce black, even if converted afterward.
        assert_eq!(pixel(&gray, 0, 0), (45, 45, 45));
        assert_eq!(gray.pixels[3], 255);
    }

    #[test]
    fn gray_knockout_group_uses_a_gray_initial_backdrop() {
        let mut parent = Canvas::new(1, 1);
        parent.fill_background(Rgb::new(0.0, 0.0, 1.0));
        let mut gray =
            Canvas::group_in_color_space(&parent, false, true, BlendColorSpace::DeviceGray);
        gray.blend(0, 0, Rgb::new(1.0, 0.0, 0.0), 1.0, None);
        gray.blend(0, 0, Rgb::new(0.0, 1.0, 0.0), 0.5, None);
        // Knock out red, then paint half-opaque green over the initial blue.
        assert_eq!(pixel(&gray, 0, 0), (89, 89, 89));
        assert_eq!(gray.group_backdrop(), &[28, 28, 28, 255]);
    }

    #[test]
    fn suppressed_canvas_does_not_paint_but_can_rasterize_a_clip() {
        let mut canvas = Canvas::new(2, 2);
        canvas.paint_suppressed = true;
        let mut path = Path::new();
        path.rect(0.0, 0.0, 2.0, 2.0);
        canvas.fill_path(&path, Rgb::BLACK, FillRule::NonZero, 1.0, None);
        canvas.blend(0, 0, Rgb::BLACK, 1.0, None);
        canvas.blend_at(1, [0.0; 3], 1.0, 1.0);
        assert!(canvas.pixels.iter().all(|&value| value == 255));
        assert_eq!(
            canvas.rasterize_mask(&path, FillRule::NonZero).data,
            vec![255; 4]
        );
    }
}
