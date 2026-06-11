//! Milestone 8 (part 2): text-positioning state machine.

/// A PDF transformation matrix [a b c d e f].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
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

    pub fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub fn translation(tx: f64, ty: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    /// self × other (PDF order: this transform applied before `other`).
    pub fn multiply(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    pub fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

/// Text state parameters (PDF 9.3).
#[derive(Debug, Clone)]
pub struct TextState {
    pub char_spacing: f64,  // Tc
    pub word_spacing: f64,  // Tw
    pub horiz_scale: f64,   // Tz (as a fraction, default 1.0)
    pub leading: f64,       // TL
    pub font_size: f64,     // Tf size
    pub rise: f64,          // Ts
    pub font_key: Option<String>,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            char_spacing: 0.0,
            word_spacing: 0.0,
            horiz_scale: 1.0,
            leading: 0.0,
            font_size: 0.0,
            rise: 0.0,
            font_key: None,
        }
    }
}

/// Live text object: text matrix + line matrix (between BT and ET).
#[derive(Debug, Clone)]
pub struct TextObject {
    pub text_matrix: Matrix,
    pub line_matrix: Matrix,
}

impl TextObject {
    pub fn new() -> Self {
        Self {
            text_matrix: Matrix::IDENTITY,
            line_matrix: Matrix::IDENTITY,
        }
    }

    /// Td — move to the start of the next line, offset by (tx, ty).
    pub fn translate_line(&mut self, tx: f64, ty: f64) {
        self.line_matrix = Matrix::translation(tx, ty).multiply(&self.line_matrix);
        self.text_matrix = self.line_matrix;
    }

    /// Tm — set both matrices.
    pub fn set_matrix(&mut self, m: Matrix) {
        self.text_matrix = m;
        self.line_matrix = m;
    }

    /// T* — next line using the current leading.
    pub fn next_line(&mut self, leading: f64) {
        self.translate_line(0.0, -leading);
    }

    /// Advance the text matrix horizontally by `tx` text-space units.
    pub fn advance(&mut self, tx: f64) {
        self.text_matrix = Matrix::translation(tx, 0.0).multiply(&self.text_matrix);
    }

    /// Device-space position of the current text origin.
    pub fn position(&self, ctm: &Matrix) -> (f64, f64) {
        let m = self.text_matrix.multiply(ctm);
        (m.e, m.f)
    }
}

impl Default for TextObject {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_moves_relative_to_line_matrix() {
        let mut t = TextObject::new();
        t.translate_line(72.0, 720.0);
        assert_eq!(t.text_matrix.e, 72.0);
        assert_eq!(t.text_matrix.f, 720.0);
        t.translate_line(0.0, -14.0);
        assert_eq!(t.text_matrix.e, 72.0);
        assert_eq!(t.text_matrix.f, 706.0);
    }

    #[test]
    fn advance_respects_scaled_matrix() {
        let mut t = TextObject::new();
        t.set_matrix(Matrix::new(2.0, 0.0, 0.0, 2.0, 10.0, 10.0));
        t.advance(5.0);
        // 5 units advance under 2x scale = 10 device units.
        assert_eq!(t.text_matrix.e, 20.0);
        assert_eq!(t.text_matrix.f, 10.0);
    }

    #[test]
    fn matrix_multiply_order() {
        let scale = Matrix::new(2.0, 0.0, 0.0, 2.0, 0.0, 0.0);
        let translate = Matrix::translation(3.0, 4.0);
        let m = translate.multiply(&scale);
        assert_eq!(m.transform_point(0.0, 0.0), (6.0, 8.0));
    }
}
