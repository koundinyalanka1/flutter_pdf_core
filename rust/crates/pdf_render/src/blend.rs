//! PDF standard blend functions (ISO 32000-1, 11.3.5).
//! The equivalent equations are also specified by W3C Compositing, section 10.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Normal" | "Compatible" => Self::Normal,
            "Multiply" => Self::Multiply,
            "Screen" => Self::Screen,
            "Overlay" => Self::Overlay,
            "Darken" => Self::Darken,
            "Lighten" => Self::Lighten,
            "ColorDodge" => Self::ColorDodge,
            "ColorBurn" => Self::ColorBurn,
            "HardLight" => Self::HardLight,
            "SoftLight" => Self::SoftLight,
            "Difference" => Self::Difference,
            "Exclusion" => Self::Exclusion,
            "Hue" => Self::Hue,
            "Saturation" => Self::Saturation,
            "Color" => Self::Color,
            "Luminosity" => Self::Luminosity,
            _ => return None,
        })
    }

    pub fn mix(self, backdrop: [f32; 3], source: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Hue => set_lum(set_sat(source, sat(backdrop)), lum(backdrop)),
            Self::Saturation => set_lum(set_sat(backdrop, sat(source)), lum(backdrop)),
            Self::Color => set_lum(source, lum(backdrop)),
            Self::Luminosity => set_lum(backdrop, lum(source)),
            _ => std::array::from_fn(|i| self.channel(backdrop[i], source[i])),
        }
    }

    fn channel(self, b: f32, s: f32) -> f32 {
        match self {
            Self::Normal => s,
            Self::Multiply => b * s,
            Self::Screen => b + s - b * s,
            Self::Overlay => Self::HardLight.channel(s, b),
            Self::Darken => b.min(s),
            Self::Lighten => b.max(s),
            Self::ColorDodge => {
                if b == 0.0 {
                    0.0
                } else if s == 1.0 {
                    1.0
                } else {
                    (b / (1.0 - s)).min(1.0)
                }
            }
            Self::ColorBurn => {
                if b == 1.0 {
                    1.0
                } else if s == 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - b) / s).min(1.0)
                }
            }
            Self::HardLight => {
                if s <= 0.5 {
                    2.0 * b * s
                } else {
                    1.0 - 2.0 * (1.0 - b) * (1.0 - s)
                }
            }
            Self::SoftLight => {
                if s <= 0.5 {
                    b - (1.0 - 2.0 * s) * b * (1.0 - b)
                } else {
                    let d = if b <= 0.25 {
                        ((16.0 * b - 12.0) * b + 4.0) * b
                    } else {
                        b.sqrt()
                    };
                    b + (2.0 * s - 1.0) * (d - b)
                }
            }
            Self::Difference => (b - s).abs(),
            Self::Exclusion => b + s - 2.0 * b * s,
            _ => unreachable!("nonseparable blend handled as a colour"),
        }
    }
}

pub fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn sat(c: [f32; 3]) -> f32 {
    c.into_iter().fold(f32::NEG_INFINITY, f32::max) - c.into_iter().fold(f32::INFINITY, f32::min)
}

fn set_lum(mut c: [f32; 3], value: f32) -> [f32; 3] {
    let delta = value - lum(c);
    c.iter_mut().for_each(|v| *v += delta);
    let l = lum(c);
    let n = c.into_iter().fold(f32::INFINITY, f32::min);
    let x = c.into_iter().fold(f32::NEG_INFINITY, f32::max);
    if n < 0.0 {
        for v in &mut c {
            *v = l + (*v - l) * l / (l - n);
        }
    }
    if x > 1.0 {
        for v in &mut c {
            *v = l + (*v - l) * (1.0 - l) / (x - l);
        }
    }
    c.map(|v| v.clamp(0.0, 1.0))
}

fn set_sat(c: [f32; 3], value: f32) -> [f32; 3] {
    let mut indices = [0, 1, 2];
    indices.sort_by(|&a, &b| c[a].total_cmp(&c[b]));
    let [min, mid, max] = indices;
    let mut out = [0.0; 3];
    if c[max] > c[min] {
        out[mid] = (c[mid] - c[min]) * value / (c[max] - c[min]);
        out[max] = value;
    }
    out
}

/// Straight-alpha source-over composition; blend functions apply only in the
/// overlap with a nontransparent backdrop, never against transparent black.
pub fn composite(pixel: &mut [u8], source: [f32; 3], alpha: f32, mode: BlendMode) {
    let a = alpha.clamp(0.0, 1.0);
    let ab = pixel[3] as f32 / 255.0;
    let ao = a + ab * (1.0 - a);
    if ao <= 0.0 {
        return;
    }
    let backdrop = std::array::from_fn(|i| pixel[i] as f32 / 255.0);
    let mixed = mode.mix(backdrop, source);
    for i in 0..3 {
        let value =
            ((1.0 - a) * ab * backdrop[i] + a * ((1.0 - ab) * source[i] + ab * mixed[i])) / ao;
        pixel[i] = (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
    pixel[3] = (ao * 255.0 + 0.5) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_modes_have_distinct_reference_results() {
        for (mode, expected) in [
            (BlendMode::Multiply, 0.16),
            (BlendMode::Screen, 0.84),
            (BlendMode::Overlay, 0.32),
            (BlendMode::Darken, 0.2),
            (BlendMode::Lighten, 0.8),
            (BlendMode::ColorDodge, 1.0),
            (BlendMode::ColorBurn, 0.0),
            (BlendMode::HardLight, 0.68),
            (BlendMode::SoftLight, 0.3488),
            (BlendMode::Difference, 0.6),
            (BlendMode::Exclusion, 0.68),
        ] {
            assert!(
                (mode.mix([0.2; 3], [0.8; 3])[0] - expected).abs() < 1e-5,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn nonseparable_modes_preserve_requested_luminosity_and_saturation() {
        let b = [0.1, 0.7, 0.4];
        let s = [0.9, 0.3, 0.2];
        for mode in [BlendMode::Hue, BlendMode::Saturation, BlendMode::Color] {
            assert!((lum(mode.mix(b, s)) - lum(b)).abs() < 1e-5);
        }
        assert!((lum(BlendMode::Luminosity.mix(b, s)) - lum(s)).abs() < 1e-5);
        assert!((sat(BlendMode::Saturation.mix(b, s)) - sat(s)).abs() < 1e-5);
    }

    #[test]
    fn transparent_backdrop_never_changes_source_colour() {
        let mut pixel = [0; 4];
        composite(&mut pixel, [1.0, 0.0, 0.0], 0.5, BlendMode::Multiply);
        assert_eq!(pixel, [255, 0, 0, 128]);
        composite(&mut pixel, [0.0, 0.0, 1.0], 0.5, BlendMode::Normal);
        assert_eq!(pixel[3], 192);
        assert!((pixel[0] as i32 - 85).abs() <= 1);
        assert!((pixel[2] as i32 - 170).abs() <= 1);
    }
}
