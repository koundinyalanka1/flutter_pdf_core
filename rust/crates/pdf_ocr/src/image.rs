//! Greyscale page images and the geometry OCR needs on them.

/// 8-bit greyscale, row-major, top-left origin: 0 is black ink, 255 white paper.
#[derive(Clone, Debug, PartialEq)]
pub struct GrayImage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl GrayImage {
    /// `None` when `pixels` does not hold exactly `width * height` samples.
    pub fn new(width: usize, height: usize, pixels: Vec<u8>) -> Option<Self> {
        (width.checked_mul(height)? == pixels.len()).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn filled(width: usize, height: usize, value: u8) -> Self {
        Self {
            width,
            height,
            pixels: vec![value; width * height],
        }
    }

    /// Luma of straight (non-premultiplied) RGBA8, composited over white so
    /// transparent regions read as paper.
    pub fn from_rgba(width: usize, height: usize, rgba: &[u8]) -> Option<Self> {
        if width.checked_mul(height)?.checked_mul(4)? != rgba.len() {
            return None;
        }
        let pixels = rgba
            .chunks_exact(4)
            .map(|p| {
                let luma =
                    (299 * u32::from(p[0]) + 587 * u32::from(p[1]) + 114 * u32::from(p[2]) + 500)
                        / 1000;
                let alpha = u32::from(p[3]);
                ((luma * alpha + 255 * (255 - alpha) + 127) / 255) as u8
            })
            .collect();
        Some(Self {
            width,
            height,
            pixels,
        })
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> u8 {
        self.pixels[y * self.width + x]
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.pixels[y * self.width..(y + 1) * self.width]
    }

    /// Resample so that what was skewed by `angle` radians becomes level.
    ///
    /// A text baseline skewed by `angle` falls by `tan(angle)` per pixel to the
    /// right (image y points down). Output pixel `p` samples the input at
    /// [`Skew::to_source`]`(p)`, bilinearly; outside the input reads as paper.
    pub fn deskewed(&self, skew: &Skew) -> GrayImage {
        let mut out = vec![255u8; self.pixels.len()];
        for y in 0..self.height {
            for x in 0..self.width {
                let (sx, sy) = skew.to_source(x as f64 + 0.5, y as f64 + 0.5);
                out[y * self.width + x] = self.sample(sx - 0.5, sy - 0.5);
            }
        }
        GrayImage {
            width: self.width,
            height: self.height,
            pixels: out,
        }
    }

    /// Turned clockwise by `turns` quarter turns, exactly (no resampling).
    pub fn turned(&self, turns: u8) -> GrayImage {
        let (w, h) = (self.width, self.height);
        let turns = turns % 4;
        if turns == 0 {
            return self.clone();
        }
        let (out_w, out_h) = if turns % 2 == 1 { (h, w) } else { (w, h) };
        let mut pixels = vec![255u8; w * h];
        for y in 0..out_h {
            for x in 0..out_w {
                let (sx, sy) = match turns {
                    1 => (y, h - 1 - x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (w - 1 - y, x),
                };
                pixels[y * out_w + x] = self.get(sx, sy);
            }
        }
        GrayImage {
            width: out_w,
            height: out_h,
            pixels,
        }
    }

    /// Bilinear sample at pixel-centre coordinates; paper outside the image.
    fn sample(&self, x: f64, y: f64) -> u8 {
        if !(x > -1.0 && y > -1.0 && x < self.width as f64 && y < self.height as f64) {
            return 255;
        }
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let at = |xi: f64, yi: f64| -> f64 {
            if xi < 0.0 || yi < 0.0 || xi >= self.width as f64 || yi >= self.height as f64 {
                255.0
            } else {
                f64::from(self.get(xi as usize, yi as usize))
            }
        };
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1.0, y0) * fx;
        let bottom = at(x0, y0 + 1.0) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0) * fx;
        (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8
    }
}

/// A rotation about the image centre that maps deskewed coordinates back to
/// the original image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Skew {
    pub angle: f64,
    cx: f64,
    cy: f64,
    cos: f64,
    sin: f64,
}

impl Skew {
    pub fn new(angle: f64, width: usize, height: usize) -> Self {
        Self {
            angle,
            cx: width as f64 / 2.0,
            cy: height as f64 / 2.0,
            cos: angle.cos(),
            sin: angle.sin(),
        }
    }

    pub fn none(width: usize, height: usize) -> Self {
        Self::new(0.0, width, height)
    }

    /// Deskewed point -> original image point.
    pub fn to_source(&self, x: f64, y: f64) -> (f64, f64) {
        let (dx, dy) = (x - self.cx, y - self.cy);
        (
            self.cx + self.cos * dx - self.sin * dy,
            self.cy + self.sin * dx + self.cos * dy,
        )
    }
}

/// Quarter turns applied to an image (see [`GrayImage::turned`]), and how
/// points in the turned image map back onto the original.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Turn {
    pub turns: u8,
    /// Size of the original image.
    width: f64,
    height: f64,
}

impl Turn {
    pub fn new(turns: u8, width: usize, height: usize) -> Self {
        Self {
            turns: turns % 4,
            width: width as f64,
            height: height as f64,
        }
    }

    /// Turned point (pixel-edge coordinates) -> original image point.
    pub fn to_source(&self, x: f64, y: f64) -> (f64, f64) {
        match self.turns {
            0 => (x, y),
            1 => (y, self.height - x),
            2 => (self.width - x, self.height - y),
            _ => (self.width - y, x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_turns_are_exact_and_map_back() {
        // 3 x 2: a b c / d e f
        let image = GrayImage::new(3, 2, vec![1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(image.turned(1).pixels, vec![4, 1, 5, 2, 6, 3]); // 2 x 3
        assert_eq!(image.turned(2).pixels, vec![6, 5, 4, 3, 2, 1]);
        assert_eq!(image.turned(3).pixels, vec![3, 6, 2, 5, 1, 4]);
        assert_eq!(image.turned(4), image);
        for turns in 0..4u8 {
            let turned = image.turned(turns);
            let turn = Turn::new(turns, 3, 2);
            for y in 0..turned.height {
                for x in 0..turned.width {
                    // The centre of each turned pixel lands on its source pixel.
                    let (sx, sy) = turn.to_source(x as f64 + 0.5, y as f64 + 0.5);
                    assert_eq!(
                        image.get(sx as usize, sy as usize),
                        turned.get(x, y),
                        "turns {turns}"
                    );
                }
            }
        }
    }

    #[test]
    fn rgba_luma_composites_over_white() {
        let rgba = [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 255, 0, 0, 255];
        let image = GrayImage::from_rgba(4, 1, &rgba).unwrap();
        assert_eq!(image.pixels, vec![0, 255, 255, 76]);
        assert!(GrayImage::from_rgba(2, 2, &rgba[..8]).is_none());
    }

    #[test]
    fn deskewing_levels_a_skewed_line() {
        // Draw a line falling 1 px per 20 px, i.e. skewed by atan(1/20).
        let (w, h) = (200, 60);
        let mut image = GrayImage::filled(w, h, 255);
        for x in 20..180 {
            let y = 25 + (x - 20) / 20;
            image.pixels[y * w + x] = 0;
            image.pixels[(y + 1) * w + x] = 0;
        }
        let skew = Skew::new((1.0f64 / 20.0).atan(), w, h);
        let level = image.deskewed(&skew);
        // Every column of the line's middle is dark on (nearly) the same rows.
        let rows: Vec<usize> = (40..160)
            .map(|x| (0..h).filter(|&y| level.get(x, y) < 128).min().unwrap())
            .collect();
        let (lo, hi) = (rows.iter().min().unwrap(), rows.iter().max().unwrap());
        assert!(hi - lo <= 1, "rows {lo}..{hi}");
        // Mapping back lands on the original line.
        let (sx, sy) = skew.to_source(100.5, rows[60] as f64 + 0.5);
        assert!(
            (sx - 100.5).abs() < 2.0 && (sy - (25.0 + 4.0 + 0.5)).abs() < 1.5,
            "{sx},{sy}"
        );
    }
}
