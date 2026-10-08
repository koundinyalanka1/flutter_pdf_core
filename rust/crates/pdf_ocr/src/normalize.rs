//! Line-image normalization: the only view of text the network ever gets.
//!
//! This is a port of `tools/ocr_train/normalize.py`, which produced every
//! training sample. The two must agree, so each step below mirrors the
//! Python one operation for operation (same f64 arithmetic for the tap
//! weights, same f32 accumulation order); `tests/golden.rs` holds them to it.

use crate::image::GrayImage;

pub const HEIGHT: usize = 32;
/// Fraction of the ink height added left and right of a line.
pub const PAD_X: f64 = 0.3;
/// Fraction of the ink height added above and below a line.
pub const PAD_Y: f64 = 0.2;
/// Inverted grey levels between paper and ink, at least.
const MIN_CONTRAST: usize = 32;

/// An integer crop rectangle, end-exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Crop {
    pub fn width(&self) -> usize {
        self.x1 - self.x0
    }

    pub fn height(&self) -> usize {
        self.y1 - self.y0
    }
}

/// Pad an ink box `[x0, y0, x1, y1]` (end-exclusive) and clamp it to the image.
pub fn crop_rect(ink: [f64; 4], width: usize, height: usize) -> Crop {
    let [x0, y0, x1, y1] = ink;
    let h = (y1 - y0).max(1.0);
    let clamp = |v: f64, max: usize| (v.max(0.0) as usize).min(max);
    Crop {
        x0: clamp((x0 - PAD_X * h).floor(), width),
        y0: clamp((y0 - PAD_Y * h).floor(), height),
        x1: clamp((x1 + PAD_X * h).ceil(), width),
        y1: clamp((y1 + PAD_Y * h).ceil(), height),
    }
}

/// A normalized line: `HEIGHT` rows of `width` floats, ink near 1, paper 0.
#[derive(Clone, Debug, PartialEq)]
pub struct LineImage {
    pub width: usize,
    pub data: Vec<f32>,
    /// The page region it was cut from.
    pub crop: Crop,
}

impl LineImage {
    /// Page x of a normalized column position (the inverse of the horizontal
    /// resampling's scale).
    pub fn page_x(&self, column: f64) -> f64 {
        self.crop.x0 as f64 + column * self.crop.width() as f64 / self.width as f64
    }
}

pub fn output_width(crop_w: usize, crop_h: usize) -> usize {
    ((crop_w as f64 * HEIGHT as f64 / crop_h as f64 + 0.5).floor() as usize).max(4)
}

pub fn normalize(image: &GrayImage, crop: Crop) -> LineImage {
    let (w, h) = (crop.width(), crop.height());
    if w == 0 || h == 0 {
        return LineImage {
            width: 4,
            data: vec![0.0; HEIGHT * 4],
            crop,
        };
    }
    let lut = contrast_lut(image, crop);
    let rows = taps(h, HEIGHT);
    let width = output_width(w, h);
    let cols = taps(w, width);

    // Vertical pass: HEIGHT rows of w samples.
    let mut vertical = vec![0f32; HEIGHT * w];
    for (out_y, out_row) in vertical.chunks_exact_mut(w).enumerate() {
        for tap in 0..rows.per {
            let weight = rows.weights[out_y * rows.per + tap];
            let source = image.row(crop.y0 + rows.indices[out_y * rows.per + tap]);
            let source = &source[crop.x0..crop.x1];
            for (acc, &g) in out_row.iter_mut().zip(source) {
                *acc += lut[g as usize] * weight;
            }
        }
    }
    // Horizontal pass.
    let mut data = vec![0f32; HEIGHT * width];
    for (out_row, in_row) in data.chunks_exact_mut(width).zip(vertical.chunks_exact(w)) {
        for (out_x, value) in out_row.iter_mut().enumerate() {
            let mut acc = 0f32;
            for tap in 0..cols.per {
                let k = out_x * cols.per + tap;
                acc += in_row[cols.indices[k]] * cols.weights[k];
            }
            *value = acc;
        }
    }
    LineImage { width, data, crop }
}

/// Grey level -> normalized ink value, from the crop's own histogram: the
/// median inverted level is paper, the 99th percentile is ink.
fn contrast_lut(image: &GrayImage, crop: Crop) -> [f32; 256] {
    let mut histogram = [0usize; 256];
    for y in crop.y0..crop.y1 {
        for &g in &image.row(y)[crop.x0..crop.x1] {
            histogram[255 - g as usize] += 1;
        }
    }
    let n = crop.width() * crop.height();
    let rank = |q: f64| -> usize {
        let k = (q * (n - 1) as f64).floor() as usize;
        let mut cumulative = 0;
        for (value, &count) in histogram.iter().enumerate() {
            cumulative += count;
            if cumulative > k {
                return value;
            }
        }
        255
    };
    let lo = rank(0.5);
    let hi = rank(0.99).max(lo + MIN_CONTRAST);
    let mut lut = [0f32; 256];
    for (g, slot) in lut.iter_mut().enumerate() {
        let inverted = (255 - g) as f64;
        *slot = ((inverted - lo as f64) / (hi - lo) as f64).clamp(0.0, 1.0) as f32;
    }
    lut
}

/// Source indices and weights per output sample; `per` taps each, padded
/// with zero weights, in increasing source order.
struct Taps {
    per: usize,
    indices: Vec<usize>,
    weights: Vec<f32>,
}

fn taps(in_len: usize, out_len: usize) -> Taps {
    if out_len < in_len {
        // Area average over [i*span, (i+1)*span).
        let span = in_len as f64 / out_len as f64;
        let per = span.ceil() as usize + 1;
        let mut indices = vec![0usize; out_len * per];
        let mut weights = vec![0f32; out_len * per];
        for i in 0..out_len {
            let start = i as f64 * span;
            let end = (i as f64 + 1.0) * span;
            let limit = end.ceil().min(in_len as f64);
            for tap in 0..per {
                let j = start.floor() + tap as f64;
                let overlap = end.min(j + 1.0) - start.max(j);
                if j < limit && overlap > 0.0 {
                    indices[i * per + tap] = j as usize;
                    weights[i * per + tap] = (overlap / span) as f32;
                }
            }
        }
        Taps {
            per,
            indices,
            weights,
        }
    } else {
        // Bilinear with pixel centres aligned, edges clamped.
        let ratio = in_len as f64 / out_len as f64;
        let last = in_len as f64 - 1.0;
        let mut indices = Vec::with_capacity(out_len * 2);
        let mut weights = Vec::with_capacity(out_len * 2);
        for i in 0..out_len {
            let source = (i as f64 + 0.5) * ratio - 0.5;
            let j0 = source.floor();
            let t = source - j0;
            indices.push(j0.clamp(0.0, last) as usize);
            indices.push((j0 + 1.0).clamp(0.0, last) as usize);
            weights.push((1.0 - t) as f32);
            weights.push(t as f32);
        }
        Taps {
            per: 2,
            indices,
            weights,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_is_padded_by_the_ink_height_and_clamped() {
        let crop = crop_rect([100.0, 50.0, 300.0, 70.0], 1000, 1000);
        assert_eq!(
            crop,
            Crop {
                x0: 94,
                y0: 46,
                x1: 306,
                y1: 74
            }
        );
        let edge = crop_rect([2.0, 1.0, 999.5, 21.0], 1000, 22);
        assert_eq!(
            edge,
            Crop {
                x0: 0,
                y0: 0,
                x1: 1000,
                y1: 22
            }
        );
    }

    #[test]
    fn dark_text_on_grey_paper_normalizes_to_ink_one_paper_zero() {
        let (w, h) = (120, 40);
        let mut image = GrayImage::filled(w, h, 200);
        for y in 15..25 {
            for x in 30..90 {
                image.pixels[y * w + x] = 20;
            }
        }
        let line = normalize(
            &image,
            Crop {
                x0: 0,
                y0: 0,
                x1: w,
                y1: h,
            },
        );
        assert_eq!((line.width, line.data.len()), (96, 96 * HEIGHT));
        let at = |x: usize, y: usize| line.data[y * line.width + x];
        assert!(at(48, 16) > 0.99, "ink {}", at(48, 16));
        assert!(at(5, 2) < 0.01, "paper {}", at(5, 2));
    }

    #[test]
    fn taps_preserve_mass_when_shrinking_and_enlarging() {
        for (n, m) in [(97, 32), (32, 32), (10, 32), (1, 4), (300, 7)] {
            let t = taps(n, m);
            for i in 0..m {
                let sum: f32 = t.weights[i * t.per..(i + 1) * t.per].iter().sum();
                assert!(
                    (sum - 1.0).abs() < 1e-5,
                    "{n}->{m} output {i} sums to {sum}"
                );
            }
        }
    }
}
