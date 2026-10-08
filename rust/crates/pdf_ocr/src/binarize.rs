//! Ink/paper separation for layout analysis (recognition sees grey levels).
//!
//! Sauvola's threshold follows uneven lighting and paper tone: a pixel is ink
//! when darker than `mean * (1 + k * (std / 128 - 1))` over the window around
//! it. Window statistics come from running column sums, so memory stays
//! proportional to one row instead of the two summed-area tables (about
//! 100 MB for an A4 page at 300 dpi) the textbook version needs.

use crate::image::GrayImage;

/// One byte per pixel: 1 is ink, 0 is paper.
#[derive(Clone, Debug, PartialEq)]
pub struct Binary {
    pub width: usize,
    pub height: usize,
    pub bits: Vec<u8>,
}

impl Binary {
    #[inline]
    pub fn is_ink(&self, x: usize, y: usize) -> bool {
        self.bits[y * self.width + x] != 0
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.bits[y * self.width..(y + 1) * self.width]
    }
}

/// An odd window of about a sixtieth of the shorter side: a few text lines
/// tall at any resolution.
pub fn default_window(width: usize, height: usize) -> usize {
    (width.min(height) / 60).clamp(15, 75) | 1
}

pub fn sauvola(image: &GrayImage, window: usize, k: f64) -> Binary {
    let (w, h) = (image.width, image.height);
    let r = window / 2;
    let mut bits = vec![0u8; w * h];
    if w == 0 || h == 0 {
        return Binary {
            width: w,
            height: h,
            bits,
        };
    }
    let mut column_sum = vec![0u64; w];
    let mut column_squares = vec![0u64; w];
    let mut prefix_sum = vec![0u64; w + 1];
    let mut prefix_squares = vec![0u64; w + 1];
    let add_row = |y: usize, sign: i64, sums: &mut [u64], squares: &mut [u64]| {
        for (x, &g) in image.row(y).iter().enumerate() {
            let (g, g2) = (u64::from(g), u64::from(g) * u64::from(g));
            if sign > 0 {
                sums[x] += g;
                squares[x] += g2;
            } else {
                sums[x] -= g;
                squares[x] -= g2;
            }
        }
    };
    for y in 0..r.min(h) {
        add_row(y, 1, &mut column_sum, &mut column_squares);
    }
    for y in 0..h {
        if y + r < h {
            add_row(y + r, 1, &mut column_sum, &mut column_squares);
        }
        if y > r {
            add_row(y - r - 1, -1, &mut column_sum, &mut column_squares);
        }
        let rows = (y + r).min(h - 1) + 1 - y.saturating_sub(r);
        for x in 0..w {
            prefix_sum[x + 1] = prefix_sum[x] + column_sum[x];
            prefix_squares[x + 1] = prefix_squares[x] + column_squares[x];
        }
        let out = &mut bits[y * w..(y + 1) * w];
        for (x, (bit, &g)) in out.iter_mut().zip(image.row(y)).enumerate() {
            let (x0, x1) = (x.saturating_sub(r), (x + r).min(w - 1));
            let n = ((x1 - x0 + 1) * rows) as f64;
            let mean = (prefix_sum[x1 + 1] - prefix_sum[x0]) as f64 / n;
            let variance = (prefix_squares[x1 + 1] - prefix_squares[x0]) as f64 / n - mean * mean;
            let threshold = mean * (1.0 + k * (variance.max(0.0).sqrt() / 128.0 - 1.0));
            *bit = u8::from(f64::from(g) < threshold);
        }
    }
    Binary {
        width: w,
        height: h,
        bits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_marks_are_ink_on_light_and_on_shaded_paper() {
        let (w, h) = (120, 60);
        let mut image = GrayImage::filled(w, h, 240);
        // Right half in shadow; a stroke in each half.
        for y in 0..h {
            for x in 60..w {
                image.pixels[y * w + x] = 150;
            }
        }
        for y in 20..40 {
            for x in [20, 21, 22, 90, 91, 92] {
                image.pixels[y * w + x] -= 120;
            }
        }
        let binary = sauvola(&image, 31, 0.2);
        assert!(binary.is_ink(21, 30) && binary.is_ink(91, 30));
        assert!(!binary.is_ink(40, 30) && !binary.is_ink(100, 30) && !binary.is_ink(75, 5));
    }
}
