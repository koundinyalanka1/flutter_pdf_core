//! Bridges a PDF font dictionary to drawable glyph outlines.
//!
//! Text rendering needs two things the text *extractor* never did: the
//! embedded font program, and a code → glyph-id mapping. This module resolves
//! `/FontFile2` (TrueType) programs and layers the PDF's own encoding rules
//! on top of the font's internal `cmap`.

pub mod cff;
pub mod fallback;
pub mod truetype;

use pdf_core::document::PdfDocument;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_text::font::{load_font, Font as TextFont};

use crate::geom::Path;
use cff::CffFont;
use fallback::{FallbackFont, FallbackStyle};
use truetype::TrueTypeFont;

/// Why a font cannot be drawn, when it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSource {
    /// Embedded TrueType outlines are available.
    TrueType,
    /// Embedded CFF / Type1C outlines are available.
    Cff,
    /// The font program is in a format this renderer does not interpret
    /// (bare Type1, for instance), so a substitute face is drawn instead.
    UnsupportedProgram,
    /// No font program embedded at all — one of the standard 14, or an
    /// external reference. A substitute face is drawn instead.
    NotEmbedded,
}

/// The embedded outline program, whichever format it turned out to be.
enum Program {
    TrueType(Box<TrueTypeFont>),
    Cff(Box<CffFont>),
}

/// A font ready for rendering: metrics from the PDF, outlines from the
/// embedded program.
pub struct RenderFont {
    /// Widths, encoding and code splitting, shared with the text extractor.
    pub text: TextFont,
    pub source: GlyphSource,
    program: Option<Program>,
    /// CID → GID table from a `/CIDToGIDMap` stream, when present.
    cid_to_gid: Option<Vec<u16>>,
    /// True for Type0 fonts, whose codes are CIDs rather than byte codes.
    composite: bool,
    /// `/Symbolic` flag from the font descriptor.
    symbolic: bool,
    /// Substitute outlines, used when nothing usable was embedded.
    fallback: Option<FallbackFont>,
}

impl RenderFont {
    pub fn load(doc: &PdfDocument, dict: &Dictionary) -> RenderFont {
        let text = load_font(doc, dict).unwrap_or_default();
        let composite = dict.get("Subtype").and_then(PdfObject::as_name) == Some("Type0");

        // For Type0, metrics and the font program live on the descendant.
        let descendant = if composite {
            descendant_font(doc, dict)
        } else {
            None
        };
        let owner = descendant.as_ref().unwrap_or(dict);

        let descriptor = owner
            .get("FontDescriptor")
            .map(|o| doc.resolve_value(o))
            .and_then(|o| doc.resolve_dict(&o).cloned());

        let symbolic = descriptor
            .as_ref()
            .and_then(|d| d.get("Flags"))
            .and_then(PdfObject::as_i64)
            .map(|flags| flags & 0b100 != 0)
            .unwrap_or(false);

        let (program, source) = match descriptor.as_ref() {
            None => (None, GlyphSource::NotEmbedded),
            Some(descriptor) => load_program(doc, descriptor),
        };

        // Nothing drawable was embedded. Rather than skip the text — which
        // renders the page blank and looks like a broken file — borrow a
        // substitute face matched to the font's declared weight and slope.
        let fallback = if program.is_none() {
            let base_font = owner
                .get("BaseFont")
                .and_then(PdfObject::as_name)
                .or_else(|| dict.get("BaseFont").and_then(PdfObject::as_name))
                .unwrap_or("");
            let flags = descriptor
                .as_ref()
                .and_then(|d| d.get("Flags"))
                .and_then(PdfObject::as_i64)
                .unwrap_or(0);
            FallbackFont::for_style(FallbackStyle::detect(base_font, flags))
        } else {
            None
        };

        let cid_to_gid = descendant
            .as_ref()
            .and_then(|d| d.get("CIDToGIDMap"))
            .map(|o| doc.resolve_value(o))
            .and_then(|object| match object {
                PdfObject::Stream(stream) => doc.stream_data(&stream).ok().map(|bytes| {
                    bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>()
                }),
                _ => None, // /Identity
            });

        RenderFont {
            text,
            source,
            program,
            cid_to_gid,
            composite,
            symbolic,
            fallback,
        }
    }

    /// The em size the outlines from [`RenderFont::outline`] are expressed
    /// in. It must track whichever source actually drew the glyph, or text
    /// scales to the wrong size — Roboto's em is 2048, not 1000.
    pub fn units_per_em(&self) -> f64 {
        match &self.program {
            Some(Program::TrueType(f)) => f.units_per_em,
            Some(Program::Cff(f)) => f.units_per_em,
            None => self
                .fallback
                .as_ref()
                .map(FallbackFont::units_per_em)
                .unwrap_or(1000.0),
        }
    }

    pub fn can_draw_glyphs(&self) -> bool {
        self.program.is_some() || self.fallback.is_some()
    }

    /// True when the glyphs being drawn are a stand-in rather than the
    /// document's own font, so a caller can note the page is approximate.
    pub fn is_substituted(&self) -> bool {
        self.program.is_none() && self.fallback.is_some()
    }

    /// Advance for one code, in text-space units (em/1000).
    ///
    /// The PDF's own `/Widths` wins whenever it has an entry, so substituted
    /// text still breaks lines where the document intended. Only when the
    /// document supplied nothing — legal for the standard 14 — does the
    /// substitute's own metric fill in, which is far closer than the flat
    /// 500-unit default it replaces.
    pub fn advance_width(&self, code: u32) -> f64 {
        if let Some(width) = self.text.explicit_width(code) {
            return width;
        }
        if let Some(fallback) = self.fallback.as_ref() {
            if let Some(ch) = self.text.decode_code(code).chars().next() {
                if let Some(advance) = fallback.advance(ch) {
                    return advance;
                }
            }
        }
        self.text.width(code)
    }

    /// Outline for one character code, in font units (y up, origin at the
    /// glyph origin). `None` when the glyph is blank or undrawable.
    pub fn outline(&self, code: u32) -> Option<Path> {
        let Some(program) = self.program.as_ref() else {
            // Substituted: the code's Unicode meaning is the only handle we
            // have on the stand-in face.
            let fallback = self.fallback.as_ref()?;
            let ch = self.text.decode_code(code).chars().next()?;
            return fallback.outline_for_char(ch);
        };
        match program {
            Program::TrueType(program) => {
                let gid = self.glyph_id(code, program)?;
                program.glyph_outline(gid)
            }
            Program::Cff(program) => {
                let gid = self.cff_glyph_id(code, program)?;
                program.glyph_outline(gid)
            }
        }
    }

    /// CFF glyph lookup.
    ///
    /// A CID-keyed CFF carries its own CID → glyph-id charset, which is the
    /// authority for a Type0 descendant; `/CIDToGIDMap` only applies to
    /// CIDFontType2 (TrueType) descendants. Simple fonts go through the
    /// encoding's glyph *name*, which is how CFF addresses glyphs natively.
    fn cff_glyph_id(&self, code: u32, program: &CffFont) -> Option<u16> {
        if self.composite {
            let cid = u16::try_from(code).ok()?;
            if let Some(gid) = program.gid_for_cid(cid) {
                return Some(gid);
            }
            return (usize::from(cid) < program.num_glyphs()).then_some(cid);
        }

        // Simple fonts: the encoding gives a character, and the character's
        // standard name is how CFF addresses the glyph.
        let decoded = self.text.decode_code(code);
        if let Some(ch) = decoded.chars().next() {
            if let Some(name) = cff::standard_name_for_char(ch) {
                if let Some(gid) = program.gid_for_name(name) {
                    return Some(gid);
                }
            }
        }
        // Subset fonts frequently have no usable charset, and are built so the
        // code is already the glyph index.
        if (code as usize) < program.num_glyphs() {
            return u16::try_from(code).ok();
        }
        None
    }

    fn glyph_id(&self, code: u32, program: &TrueTypeFont) -> Option<u16> {
        if self.composite {
            // Identity-H: the code *is* the CID.
            let cid = code as usize;
            return match &self.cid_to_gid {
                Some(table) => table.get(cid).copied(),
                None => Some(code as u16),
            };
        }

        // Simple fonts: symbolic ones address the font's own (3,0) cmap
        // directly; non-symbolic ones go code → char → cmap.
        if self.symbolic {
            if let Some(gid) = program.glyph_for_char(0xF000 + (code & 0xFF)) {
                return Some(gid);
            }
            if let Some(gid) = program.glyph_for_char(code) {
                return Some(gid);
            }
        }
        let decoded = self.text.decode_code(code);
        if let Some(ch) = decoded.chars().next() {
            if let Some(gid) = program.glyph_for_char(ch as u32) {
                return Some(gid);
            }
        }
        if let Some(gid) = program.glyph_for_char(code) {
            return Some(gid);
        }
        // Last resort for subset fonts with no usable cmap: many are built so
        // that the code is already the glyph index.
        if code < program.num_glyphs() as u32 {
            return Some(code as u16);
        }
        None
    }
}

/// Parse whichever font program the descriptor embeds.
///
/// `/FontFile2` is TrueType and `/FontFile3` is CFF, except that a
/// `/Subtype /OpenType` program can be either — an sfnt wrapper holding
/// `glyf` *or* `CFF `. So the bytes are tried as TrueType first and fall
/// through to CFF, which knows how to unwrap an OpenType container.
fn load_program(doc: &PdfDocument, descriptor: &Dictionary) -> (Option<Program>, GlyphSource) {
    let Some((data, prefer_truetype)) = font_program(doc, descriptor) else {
        return (None, GlyphSource::NotEmbedded);
    };

    if prefer_truetype {
        if let Some(font) = TrueTypeFont::parse(data.clone()) {
            if font.has_outlines() {
                return (Some(Program::TrueType(Box::new(font))), GlyphSource::TrueType);
            }
        }
    }
    if let Some(font) = CffFont::parse(data.clone()) {
        if font.num_glyphs() > 0 {
            return (Some(Program::Cff(Box::new(font))), GlyphSource::Cff);
        }
    }
    if !prefer_truetype {
        if let Some(font) = TrueTypeFont::parse(data) {
            if font.has_outlines() {
                return (Some(Program::TrueType(Box::new(font))), GlyphSource::TrueType);
            }
        }
    }
    (None, GlyphSource::UnsupportedProgram)
}

fn descendant_font(doc: &PdfDocument, dict: &Dictionary) -> Option<Dictionary> {
    let descendants = doc.resolve_value(dict.get("DescendantFonts")?);
    let first = match descendants {
        PdfObject::Array(items) => items.into_iter().next()?,
        other => other,
    };
    let resolved = doc.resolve_value(&first);
    doc.resolve_dict(&resolved).cloned()
}

/// Returns `(bytes, is_truetype)` for an embedded font program.
fn font_program(doc: &PdfDocument, descriptor: &Dictionary) -> Option<(Vec<u8>, bool)> {
    for (key, is_truetype) in [("FontFile2", true), ("FontFile3", false), ("FontFile", false)] {
        let Some(entry) = descriptor.get(key) else {
            continue;
        };
        let resolved = doc.resolve_value(entry);
        if let PdfObject::Stream(stream) = resolved {
            // FontFile3 may still carry OpenType data (/Subtype /OpenType).
            let subtype = stream
                .dictionary
                .get("Subtype")
                .and_then(PdfObject::as_name)
                .unwrap_or("");
            let truetype = is_truetype || subtype == "OpenType";
            if let Ok(data) = doc.stream_data(&stream) {
                return Some((data, truetype));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_core::object::PdfObject;

    #[test]
    fn missing_descriptor_draws_a_substitute() {
        let doc = PdfDocument::new_empty("1.7");
        let mut dict = Dictionary::new();
        dict.insert("Type".into(), PdfObject::Name("Font".into()));
        dict.insert("Subtype".into(), PdfObject::Name("Type1".into()));
        dict.insert("BaseFont".into(), PdfObject::Name("Helvetica".into()));
        let font = RenderFont::load(&doc, &dict);
        // Still honestly reported as not embedded...
        assert_eq!(font.source, GlyphSource::NotEmbedded);
        // ...but it draws, because a blank page is the worse answer.
        assert!(font.can_draw_glyphs());
        assert!(font.is_substituted());
        assert!(font.outline(65).is_some(), "'A' should have an outline");
    }

    #[test]
    fn units_per_em_follows_the_substitute_that_drew_the_glyph() {
        let doc = PdfDocument::new_empty("1.7");
        let mut dict = Dictionary::new();
        dict.insert("BaseFont".into(), PdfObject::Name("Helvetica".into()));
        let font = RenderFont::load(&doc, &dict);
        // Roboto's em is 2048; reporting 1000 here would scale text wrongly.
        assert_eq!(font.units_per_em(), font.fallback.as_ref().unwrap().units_per_em());
        assert!(font.units_per_em() > 0.0);
    }

    #[test]
    fn document_widths_win_over_the_substitutes_own_metrics() {
        let doc = PdfDocument::new_empty("1.7");
        let mut dict = Dictionary::new();
        dict.insert("Subtype".into(), PdfObject::Name("Type1".into()));
        dict.insert("BaseFont".into(), PdfObject::Name("Helvetica".into()));
        dict.insert("FirstChar".into(), PdfObject::Integer(65));
        dict.insert(
            "Widths".into(),
            PdfObject::Array(vec![PdfObject::Integer(722)]),
        );
        let font = RenderFont::load(&doc, &dict);
        assert_eq!(font.advance_width(65), 722.0);
        // 'B' has no /Widths entry, so the substitute fills in — and must not
        // return the flat 500 default that used to pile glyphs up.
        let b = font.advance_width(66);
        assert!(b > 0.0 && b != 500.0, "expected a real metric, got {b}");
    }
}
