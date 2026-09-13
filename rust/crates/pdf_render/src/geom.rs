//! 2-D affine transforms and flattened paths.
//!
//! PDF's matrix is `[a b c d e f]`, applied as a row vector:
//! `(x' y' 1) = (x y 1) · [[a b 0] [c d 0] [e f 1]]`.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub const fn translate(tx: f64, ty: f64) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    pub const fn scale(sx: f64, sy: f64) -> Self {
        Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// `self` then `other` (i.e. `self × other` in PDF's row-vector order).
    pub fn then(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Transform a direction (ignores translation).
    pub fn apply_vector(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y, self.b * x + self.d * y)
    }

    pub fn determinant(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    pub fn invert(&self) -> Option<Matrix> {
        let det = self.determinant();
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Matrix {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        })
    }

    /// Approximate uniform scale factor — used to pick a curve flattening
    /// tolerance and to scale line widths.
    pub fn mean_scale(&self) -> f64 {
        let sx = (self.a * self.a + self.b * self.b).sqrt();
        let sy = (self.c * self.c + self.d * self.d).sqrt();
        ((sx * sy).abs()).sqrt().max(1e-6)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// A path already flattened to polylines. Curves are subdivided at
/// construction time, which keeps the rasterizer free of curve maths.
#[derive(Debug, Clone, Default)]
pub struct Path {
    pub subpaths: Vec<SubPath>,
    current: Option<Point>,
    start: Option<Point>,
    /// Device-space tolerance used when flattening curves.
    tolerance: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SubPath {
    pub points: Vec<Point>,
    pub closed: bool,
}

impl Path {
    pub fn new() -> Self {
        Self {
            tolerance: 0.2,
            ..Default::default()
        }
    }

    pub fn with_tolerance(tolerance: f64) -> Self {
        Self {
            tolerance: tolerance.max(0.01),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.iter().all(|s| s.points.len() < 2)
    }

    pub fn current_point(&self) -> Option<Point> {
        self.current
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        let point = Point::new(x, y);
        self.subpaths.push(SubPath {
            points: vec![point],
            closed: false,
        });
        self.current = Some(point);
        self.start = Some(point);
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        if self.subpaths.is_empty() {
            self.move_to(x, y);
            return;
        }
        let point = Point::new(x, y);
        if let Some(subpath) = self.subpaths.last_mut() {
            subpath.points.push(point);
        }
        self.current = Some(point);
    }

    /// Cubic Bézier, flattened adaptively.
    pub fn curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) {
        let p0 = self.current.unwrap_or(Point::new(x1, y1));
        if self.subpaths.is_empty() {
            self.move_to(p0.x, p0.y);
        }
        let steps = cubic_steps(
            p0,
            Point::new(x1, y1),
            Point::new(x2, y2),
            Point::new(x3, y3),
            self.tolerance,
        );
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let mt = 1.0 - t;
            let x = mt * mt * mt * p0.x
                + 3.0 * mt * mt * t * x1
                + 3.0 * mt * t * t * x2
                + t * t * t * x3;
            let y = mt * mt * mt * p0.y
                + 3.0 * mt * mt * t * y1
                + 3.0 * mt * t * t * y2
                + t * t * t * y3;
            self.line_to(x, y);
        }
        self.current = Some(Point::new(x3, y3));
    }

    /// Quadratic Bézier — TrueType outlines are quadratic.
    pub fn quad_to(&mut self, cx: f64, cy: f64, x: f64, y: f64) {
        let p0 = self.current.unwrap_or(Point::new(cx, cy));
        // Elevate to cubic and reuse the adaptive flattener.
        self.curve_to(
            p0.x + 2.0 / 3.0 * (cx - p0.x),
            p0.y + 2.0 / 3.0 * (cy - p0.y),
            x + 2.0 / 3.0 * (cx - x),
            y + 2.0 / 3.0 * (cy - y),
            x,
            y,
        );
    }

    pub fn close(&mut self) {
        if let Some(subpath) = self.subpaths.last_mut() {
            subpath.closed = true;
        }
        self.current = self.start;
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
    }

    pub fn transform(&self, matrix: &Matrix) -> Path {
        Path {
            subpaths: self
                .subpaths
                .iter()
                .map(|subpath| SubPath {
                    points: subpath
                        .points
                        .iter()
                        .map(|p| {
                            let (x, y) = matrix.apply(p.x, p.y);
                            Point::new(x, y)
                        })
                        .collect(),
                    closed: subpath.closed,
                })
                .collect(),
            current: None,
            start: None,
            tolerance: self.tolerance,
        }
    }

    pub fn extend(&mut self, other: &Path) {
        self.subpaths.extend(other.subpaths.iter().cloned());
    }

    /// Device-space bounds as `(min_x, min_y, max_x, max_y)`.
    pub fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let mut bounds: Option<(f64, f64, f64, f64)> = None;
        for subpath in &self.subpaths {
            for p in &subpath.points {
                bounds = Some(match bounds {
                    None => (p.x, p.y, p.x, p.y),
                    Some((x0, y0, x1, y1)) => {
                        (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y))
                    }
                });
            }
        }
        bounds
    }
}

/// Pick a subdivision count from the curve's "flatness" — the distance of the
/// control points from the chord bounds how far the polyline can deviate.
fn cubic_steps(p0: Point, p1: Point, p2: Point, p3: Point, tolerance: f64) -> usize {
    let d1 = (p1.x - p0.x).hypot(p1.y - p0.y);
    let d2 = (p2.x - p1.x).hypot(p2.y - p1.y);
    let d3 = (p3.x - p2.x).hypot(p3.y - p2.y);
    let length = d1 + d2 + d3;
    ((length / tolerance.max(0.01)).sqrt().ceil() as usize).clamp(1, 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_composition_matches_pdf_order() {
        let scale = Matrix::scale(2.0, 3.0);
        let translate = Matrix::translate(10.0, 20.0);
        // Scale first, then translate.
        let combined = scale.then(&translate);
        assert_eq!(combined.apply(1.0, 1.0), (12.0, 23.0));
    }

    #[test]
    fn inverse_round_trips() {
        let m = Matrix::new(2.0, 0.5, -0.25, 3.0, 7.0, -4.0);
        let inv = m.invert().unwrap();
        let (x, y) = m.apply(3.0, 5.0);
        let (rx, ry) = inv.apply(x, y);
        assert!((rx - 3.0).abs() < 1e-9);
        assert!((ry - 5.0).abs() < 1e-9);
    }

    #[test]
    fn singular_matrix_has_no_inverse() {
        assert!(Matrix::scale(0.0, 1.0).invert().is_none());
    }

    #[test]
    fn rect_produces_a_closed_square() {
        let mut path = Path::new();
        path.rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(path.subpaths.len(), 1);
        assert!(path.subpaths[0].closed);
        assert_eq!(path.bounds(), Some((0.0, 0.0, 10.0, 10.0)));
    }

    #[test]
    fn curves_flatten_within_tolerance() {
        let mut path = Path::with_tolerance(0.1);
        path.move_to(0.0, 0.0);
        path.curve_to(0.0, 100.0, 100.0, 100.0, 100.0, 0.0);
        // The apex of this symmetric curve sits at y = 75.
        let peak = path.subpaths[0]
            .points
            .iter()
            .map(|p| p.y)
            .fold(f64::MIN, f64::max);
        assert!((peak - 75.0).abs() < 1.0, "peak was {peak}");
    }
}
