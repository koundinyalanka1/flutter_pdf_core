//! Scanline rasterizer and RGBA canvas.
//!
//! Anti-aliasing is 4 sub-scanlines per pixel row with exact horizontal span
//! coverage. That gives clean text and hairlines at thumbnail sizes without
//! the memory cost of full supersampling, and keeps the inner loop to simple
//! f32 accumulation.

use std::rc::Rc;

use crate::geom::{FillRule, Path, Point};

/// Sub-scanlines sampled per pixel row.
const SUB_SAMPLES: usize = 4;

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

/// RGBA8 output buffer. Alpha is always 255 — a rendered page is opaque.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// Straight RGBA, row-major, top-left origin.
    pub pixels: Vec<u8>,
    /// Reusable coverage row so filling does not allocate per scanline.
    coverage: Vec<f32>,
    crossings: Vec<(f64, i32)>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![255; width * height * 4],
            coverage: vec![0.0; width + 2],
            crossings: Vec::with_capacity(64),
        }
    }

    pub fn fill_background(&mut self, color: Rgb) {
        let r = to_u8(color.r);
        let g = to_u8(color.g);
        let b = to_u8(color.b);
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
        if alpha <= 0.001 {
            return;
        }
        self.scan(path, rule, |canvas, y, row| {
            canvas.blend_row(y, row, color, alpha, clip);
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
        self.scan(path, rule, |_canvas, y, row| {
            let base = y * width;
            for x in 0..width {
                let c = row[x].clamp(0.0, 1.0);
                if c > 0.0 {
                    mask.data[base + x] = (c * 255.0 + 0.5) as u8;
                }
            }
        });
        mask
    }

    /// Shared scanline walk. Calls `emit(self, y, coverage_row)` per pixel row
    /// that the path touches.
    fn scan<F>(&mut self, path: &Path, rule: FillRule, mut emit: F)
    where
        F: FnMut(&mut Canvas, usize, &[f32]),
    {
        let edges = collect_edges(path);
        if edges.is_empty() {
            return;
        }

        let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
        for edge in &edges {
            min_y = min_y.min(edge.y0.min(edge.y1));
            max_y = max_y.max(edge.y0.max(edge.y1));
        }
        let y_start = (min_y.floor().max(0.0)) as usize;
        let y_end = (max_y.ceil().min(self.height as f64)) as usize;
        if y_start >= y_end {
            return;
        }

        let width = self.width;
        let sub_weight = 1.0 / SUB_SAMPLES as f32;

        for y in y_start..y_end {
            // Reset only the span we may have touched last time.
            for value in self.coverage.iter_mut() {
                *value = 0.0;
            }
            let mut touched = false;

            for sub in 0..SUB_SAMPLES {
                let sample_y = y as f64 + (sub as f64 + 0.5) / SUB_SAMPLES as f64;
                self.crossings.clear();
                for edge in &edges {
                    if sample_y >= edge.y_min && sample_y < edge.y_max {
                        let t = (sample_y - edge.y0) / (edge.y1 - edge.y0);
                        self.crossings
                            .push((edge.x0 + t * (edge.x1 - edge.x0), edge.winding));
                    }
                }
                if self.crossings.len() < 2 {
                    continue;
                }
                self.crossings
                    .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

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
                    if add_span(&mut self.coverage, width, x_from, x_to, sub_weight) {
                        touched = true;
                    }
                }
            }

            if touched {
                // Split the borrow: `emit` needs &mut Canvas while reading the row.
                let row = std::mem::take(&mut self.coverage);
                emit(self, y, &row);
                self.coverage = row;
            }
        }
    }

    fn blend_row(
        &mut self,
        y: usize,
        coverage: &[f32],
        color: Rgb,
        alpha: f32,
        clip: Option<&Mask>,
    ) {
        let r = color.r.clamp(0.0, 1.0);
        let g = color.g.clamp(0.0, 1.0);
        let b = color.b.clamp(0.0, 1.0);
        let row_base = y * self.width;
        for x in 0..self.width {
            let mut a = coverage[x].clamp(0.0, 1.0) * alpha;
            if a <= 0.002 {
                continue;
            }
            if let Some(mask) = clip {
                a *= mask.data[row_base + x] as f32 / 255.0;
                if a <= 0.002 {
                    continue;
                }
            }
            let offset = (row_base + x) * 4;
            blend_pixel(&mut self.pixels[offset..offset + 4], r, g, b, a);
        }
    }

    /// Blend a single device pixel — used by the image painter.
    pub fn blend(&mut self, x: usize, y: usize, color: Rgb, alpha: f32, clip: Option<&Mask>) {
        if x >= self.width || y >= self.height || alpha <= 0.002 {
            return;
        }
        let index = y * self.width + x;
        let mut a = alpha;
        if let Some(mask) = clip {
            a *= mask.data[index] as f32 / 255.0;
            if a <= 0.002 {
                return;
            }
        }
        let offset = index * 4;
        blend_pixel(
            &mut self.pixels[offset..offset + 4],
            color.r.clamp(0.0, 1.0),
            color.g.clamp(0.0, 1.0),
            color.b.clamp(0.0, 1.0),
            a,
        );
    }
}

fn blend_pixel(pixel: &mut [u8], r: f32, g: f32, b: f32, a: f32) {
    let inv = 1.0 - a;
    pixel[0] = to_u8(pixel[0] as f32 / 255.0 * inv + r * a);
    pixel[1] = to_u8(pixel[1] as f32 / 255.0 * inv + g * a);
    pixel[2] = to_u8(pixel[2] as f32 / 255.0 * inv + b * a);
    pixel[3] = 255;
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Accumulate horizontal coverage for `[x_from, x_to)` on one sub-scanline.
/// Returns whether anything landed inside the canvas.
fn add_span(coverage: &mut [f32], width: usize, x_from: f64, x_to: f64, weight: f32) -> bool {
    let left = x_from.max(0.0);
    let right = x_to.min(width as f64);
    if right <= left {
        return false;
    }
    let first = left.floor() as usize;
    let last = (right.ceil() as usize).min(width);
    if first >= width {
        return false;
    }

    if last - first == 1 {
        // Span sits inside a single pixel.
        coverage[first] += weight * (right - left) as f32;
        return true;
    }
    // Partial first pixel, solid middle, partial last pixel.
    coverage[first] += weight * ((first + 1) as f64 - left) as f32;
    for x in (first + 1)..last.saturating_sub(1) {
        coverage[x] += weight;
    }
    if last >= 1 && last - 1 > first {
        coverage[last - 1] += weight * (right - (last - 1) as f64) as f32;
    }
    true
}

struct Edge {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
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
    let winding = if end.y > start.y { 1 } else { -1 };
    edges.push(Edge {
        x0: start.x,
        y0: start.y,
        x1: end.x,
        y1: end.y,
        y_min: start.y.min(end.y),
        y_max: start.y.max(end.y),
        winding,
    });
}

/// Convert a path into a fillable outline approximating a stroke of
/// `width` device units. Each segment becomes a quad; joins and caps get a
/// polygon disc, which is visually indistinguishable from round joins at the
/// sizes this renderer targets.
pub fn stroke_outline(path: &Path, width: f64) -> Path {
    let half = (width / 2.0).max(0.35); // never thinner than a hairline
    let mut out = Path::new();

    for subpath in &path.subpaths {
        let mut points = subpath.points.clone();
        if subpath.closed && points.len() > 1 {
            points.push(points[0]);
        }
        if points.len() == 1 {
            push_disc(&mut out, points[0], half);
            continue;
        }
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let length = dx.hypot(dy);
            if length < 1e-9 {
                continue;
            }
            let nx = -dy / length * half;
            let ny = dx / length * half;
            out.move_to(a.x + nx, a.y + ny);
            out.line_to(b.x + nx, b.y + ny);
            out.line_to(b.x - nx, b.y - ny);
            out.line_to(a.x - nx, a.y - ny);
            out.close();
        }
        // Joins and caps.
        if half > 0.6 {
            for point in &points {
                push_disc(&mut out, *point, half);
            }
        }
    }
    out
}

fn push_disc(path: &mut Path, center: Point, radius: f64) {
    const SEGMENTS: usize = 8;
    for i in 0..SEGMENTS {
        let angle = i as f64 / SEGMENTS as f64 * std::f64::consts::TAU;
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
        assert!(r > 0 && r < 255, "half-covered pixel should be grey, got {r}");
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
}
