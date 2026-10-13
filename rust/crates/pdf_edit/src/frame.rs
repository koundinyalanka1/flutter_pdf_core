//! From the page as it is shown (points, top-left origin, after the crop
//! box and /Rotate) to the page's own user space, where annotations and
//! content live.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::ObjectId;
use pdf_text::layout::page_geometry;
use pdf_text::text_state::Matrix;

pub struct Frame {
    /// The displayed size.
    pub width: f64,
    pub height: f64,
    to_user: Matrix,
    to_display: Matrix,
}

impl Frame {
    pub fn of(doc: &PdfDocument, page: ObjectId) -> Result<Frame> {
        let geometry = page_geometry(doc, page)?;
        let to_user = geometry
            .transform
            .invert()
            .ok_or_else(|| PdfError::Structure("page transform is not invertible".into()))?;
        Ok(Frame {
            width: geometry.width,
            height: geometry.height,
            to_user,
            to_display: geometry.transform,
        })
    }

    /// A displayed point in user space.
    pub fn user(&self, x: f64, y: f64) -> (f64, f64) {
        self.to_user.transform_point(x, y)
    }

    /// A user-space point as displayed.
    pub fn display(&self, x: f64, y: f64) -> (f64, f64) {
        self.to_display.transform_point(x, y)
    }

    /// A displayed `[left, top, right, bottom]` as a user-space
    /// `[x0, y0, x1, y1]`.
    pub fn user_rect(&self, r: [f64; 4]) -> [f64; 4] {
        let corners = [self.user(r[0], r[1]), self.user(r[2], r[1]), self.user(r[0], r[3]), self.user(r[2], r[3])];
        bounds(&corners)
    }

    /// A user-space rectangle as displayed `[left, top, right, bottom]`.
    pub fn display_rect(&self, r: [f64; 4]) -> [f64; 4] {
        let corners = [self.display(r[0], r[1]), self.display(r[2], r[1]), self.display(r[0], r[3]), self.display(r[2], r[3])];
        bounds(&corners)
    }

    /// Where something drawn upright in a displayed box lands: from a
    /// space with its origin at the box's bottom-left corner, y up, to
    /// user space.
    pub fn upright(&self, r: [f64; 4]) -> Matrix {
        let (left, bottom) = (r[0].min(r[2]), r[1].max(r[3]));
        Matrix::new(1.0, 0.0, 0.0, -1.0, left, bottom).multiply(&self.to_user)
    }
}

/// `[x0, y0, x1, y1]` around points.
pub fn bounds(points: &[(f64, f64)]) -> [f64; 4] {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for &(x, y) in points {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
    if !b[0].is_finite() {
        return [0.0; 4];
    }
    b
}

/// A displayed rectangle with its corners in order.
pub fn normal(r: [f64; 4]) -> [f64; 4] {
    [r[0].min(r[2]), r[1].min(r[3]), r[0].max(r[2]), r[1].max(r[3])]
}
