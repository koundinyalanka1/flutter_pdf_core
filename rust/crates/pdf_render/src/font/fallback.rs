//! Substitute outlines for fonts a PDF does not embed.
//!
//! A PDF is allowed to name any of the 14 standard fonts — Helvetica, Times,
//! Courier, Symbol, ZapfDingbats — and embed nothing, on the understanding
//! that the reader already has them. It is also allowed to embed a font
//! program in a format a given reader cannot parse. In both cases this
//! renderer used to draw nothing at all, which turned ordinary documents into
//! blank white pages.
//!
//! Roboto stands in for all of them. It is not metrically identical to
//! Helvetica or Times, but that matters less than it sounds: advances come
//! from the PDF's own `/Widths` array whenever there is one, so glyphs land
//! where the document says they should and only their shapes differ. Where
//! `/Widths` is absent, [`FallbackFont::advance`] supplies Roboto's own
//! metrics, which beats the flat 500-unit guess that came before.
//!
//! Bold is a second embedded face. Italic is sheared from the upright rather
//! than embedded, which keeps two font files out of the binary for a
//! difference few readers would notice at body-text size.

use std::sync::OnceLock;

use super::truetype::TrueTypeFont;
use crate::geom::{Matrix, Path};

const ROBOTO_REGULAR: &[u8] = include_bytes!("../../fonts/Roboto-Regular.ttf");
const ROBOTO_BOLD: &[u8] = include_bytes!("../../fonts/Roboto-Bold.ttf");

/// How much to lean a synthesised italic, as a shear on x per unit y.
/// 0.21 is about 12 degrees, the slope most oblique faces use.
const OBLIQUE_SHEAR: f64 = 0.21;

/// Which substitute face a font dictionary should borrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackStyle {
    pub bold: bool,
    pub italic: bool,
    /// Courier and friends: every glyph advances 600/1000 em.
    pub fixed_pitch: bool,
}

impl FallbackStyle {
    /// Read weight and slope out of a `/BaseFont` name and the descriptor
    /// flags. Names look like `AAAAAA+Helvetica-BoldOblique`, and the style
    /// words are the only part worth trusting.
    pub fn detect(base_font: &str, flags: i64) -> FallbackStyle {
        let name = base_font.to_ascii_lowercase();
        let bold = name.contains("bold")
            || name.contains("black")
            || name.contains("heavy")
            || name.contains("semibold")
            // Bit 19 (1 << 18) is ForceBold.
            || flags & (1 << 18) != 0;
        let italic = name.contains("italic")
            || name.contains("oblique")
            // Bit 7 (1 << 6) is Italic.
            || flags & (1 << 6) != 0;
        let fixed_pitch = name.contains("courier")
            || name.contains("mono")
            // Bit 1 is FixedPitch.
            || flags & 1 != 0;
        FallbackStyle {
            bold,
            italic,
            fixed_pitch,
        }
    }
}

/// A parsed substitute face. Both faces are parsed at most once per process.
pub struct FallbackFont {
    font: &'static TrueTypeFont,
    style: FallbackStyle,
}

fn regular() -> Option<&'static TrueTypeFont> {
    static CELL: OnceLock<Option<TrueTypeFont>> = OnceLock::new();
    CELL.get_or_init(|| TrueTypeFont::parse(ROBOTO_REGULAR.to_vec()))
        .as_ref()
}

fn bold() -> Option<&'static TrueTypeFont> {
    static CELL: OnceLock<Option<TrueTypeFont>> = OnceLock::new();
    CELL.get_or_init(|| TrueTypeFont::parse(ROBOTO_BOLD.to_vec()))
        .as_ref()
}

impl FallbackFont {
    pub fn for_style(style: FallbackStyle) -> Option<FallbackFont> {
        let font = if style.bold { bold() } else { regular() }?;
        if !font.has_outlines() {
            return None;
        }
        Some(FallbackFont { font, style })
    }

    pub fn units_per_em(&self) -> f64 {
        self.font.units_per_em
    }

    /// Outline for a Unicode character, sheared when a slanted face was asked
    /// for. `None` when the substitute has no glyph for it either.
    pub fn outline_for_char(&self, ch: char) -> Option<Path> {
        let gid = self.font.glyph_for_char(ch as u32)?;
        let outline = self.font.glyph_outline(gid)?;
        if !self.style.italic {
            return Some(outline);
        }
        Some(outline.transform(&Matrix::new(
            1.0,
            0.0,
            OBLIQUE_SHEAR,
            1.0,
            0.0,
            0.0,
        )))
    }

    /// Advance for a character, in text-space units (em/1000), for use only
    /// when the PDF gave no width of its own.
    pub fn advance(&self, ch: char) -> Option<f64> {
        if self.style.fixed_pitch {
            return Some(600.0);
        }
        let gid = self.font.glyph_for_char(ch as u32)?;
        let advance = self.font.advance(gid)?;
        let upem = self.font.units_per_em;
        if upem <= 0.0 {
            return None;
        }
        Some(advance * 1000.0 / upem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_faces_parse() {
        assert!(regular().is_some(), "Roboto-Regular should parse");
        assert!(bold().is_some(), "Roboto-Bold should parse");
    }

    #[test]
    fn draws_latin_letters() {
        let font = FallbackFont::for_style(FallbackStyle {
            bold: false,
            italic: false,
            fixed_pitch: false,
        })
        .expect("fallback available");
        for ch in ['A', 'g', '7', '%'] {
            let outline = font.outline_for_char(ch).expect("outline should exist");
            assert!(
                !outline.is_empty(),
                "{ch} should have a non-empty outline"
            );
        }
        // A space is a real glyph with no contours; that is not a failure.
        assert!(font.advance(' ').unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn style_detection_reads_subset_prefixed_names() {
        let s = FallbackStyle::detect("ABCDEF+Helvetica-BoldOblique", 0);
        assert!(s.bold && s.italic && !s.fixed_pitch);

        let s = FallbackStyle::detect("Times-Roman", 0);
        assert!(!s.bold && !s.italic && !s.fixed_pitch);

        let s = FallbackStyle::detect("Courier", 0);
        assert!(s.fixed_pitch);

        // Descriptor flags stand in when the name says nothing.
        let s = FallbackStyle::detect("SomeFont", (1 << 18) | (1 << 6) | 1);
        assert!(s.bold && s.italic && s.fixed_pitch);
    }

    #[test]
    fn courier_advances_are_monospaced() {
        let font = FallbackFont::for_style(FallbackStyle {
            bold: false,
            italic: false,
            fixed_pitch: true,
        })
        .expect("fallback available");
        assert_eq!(font.advance('i'), Some(600.0));
        assert_eq!(font.advance('W'), Some(600.0));
    }

    #[test]
    fn proportional_advances_differ_by_glyph() {
        let font = FallbackFont::for_style(FallbackStyle {
            bold: false,
            italic: false,
            fixed_pitch: false,
        })
        .expect("fallback available");
        let narrow = font.advance('i').unwrap();
        let wide = font.advance('W').unwrap();
        assert!(wide > narrow, "W ({wide}) should be wider than i ({narrow})");
    }

    #[test]
    fn italic_shear_moves_upper_outline_right() {
        let upright = FallbackFont::for_style(FallbackStyle {
            bold: false,
            italic: false,
            fixed_pitch: false,
        })
        .unwrap()
        .outline_for_char('l')
        .unwrap();
        let slanted = FallbackFont::for_style(FallbackStyle {
            bold: false,
            italic: true,
            fixed_pitch: false,
        })
        .unwrap()
        .outline_for_char('l')
        .unwrap();
        let max_x = |p: &Path| p.bounds().expect("glyph has bounds").2;
        assert!(max_x(&slanted) > max_x(&upright));
    }
}
