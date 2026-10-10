//! Bridges a PDF font dictionary to drawable glyph outlines.
//!
//! Text rendering needs two things the text *extractor* never did: the
//! embedded font program, and a code → glyph mapping. This module resolves
//! `/FontFile` (Type 1), `/FontFile2` (TrueType) and `/FontFile3` (CFF,
//! OpenType) programs and layers the PDF's own encoding rules on top of each
//! program's internal addressing — names for Type 1 and CFF, a `cmap` for
//! TrueType.

pub mod cff;
pub mod fallback;
pub mod system;
pub mod truetype;
pub mod type1;

use std::collections::HashMap;

use pdf_core::document::PdfDocument;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_text::font::{load_font, Font as TextFont};

use crate::geom::{Matrix, Path};
use cff::CffFont;
use fallback::{CjkRegion, FallbackFont, FallbackStyle};
use truetype::TrueTypeFont;
use type1::Type1Font;

/// Why a font cannot be drawn, when it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSource {
    /// Embedded TrueType outlines are available.
    TrueType,
    /// Embedded CFF / Type1C outlines are available.
    Cff,
    /// An embedded Type 1 (PostScript) program is available.
    Type1,
    /// The font program is in a format this renderer cannot read, or is
    /// damaged, so a substitute face is drawn instead.
    UnsupportedProgram,
    /// No font program embedded at all — one of the standard 14, or an
    /// external reference. A substitute face is drawn instead.
    NotEmbedded,
    /// A Type3 font: each glyph is a small content stream in `/CharProcs`,
    /// drawn by running it, not by filling an outline.
    Type3,
}

/// A Type3 font's glyph procedures and the space they are written in.
pub struct Type3Glyphs {
    /// Glyph space → text space. Unlike every other font type this is not a
    /// fixed 1/1000 scale; TeX bitmap fonts in particular use odd values.
    pub font_matrix: Matrix,
    /// Glyph name → its content stream.
    char_procs: HashMap<String, PdfStream>,
    /// The font's own `/Resources`, for procedures that name images, fonts
    /// or colour spaces. Absent means "use the page's", as PDF 1.1 allowed.
    pub resources: Option<Dictionary>,
}

impl Type3Glyphs {
    fn load(doc: &PdfDocument, dict: &Dictionary) -> Option<Type3Glyphs> {
        let numbers: Vec<f64> = match dict.get("FontMatrix").map(|o| doc.resolve_value(o)) {
            Some(PdfObject::Array(items)) => items
                .iter()
                .filter_map(|item| match doc.resolve_value(item) {
                    PdfObject::Integer(n) => Some(n as f64),
                    PdfObject::Real(n) => Some(n),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        // Required by the spec; 0.001 scaling is the conventional default
        // for the rare writer that leaves it out.
        let font_matrix = match numbers.as_slice() {
            &[a, b, c, d, e, f] if numbers.iter().all(|n| n.is_finite()) => {
                Matrix::new(a, b, c, d, e, f)
            }
            _ => Matrix::scale(0.001, 0.001),
        };
        let procs = dict
            .get("CharProcs")
            .map(|o| doc.resolve_value(o))
            .and_then(|o| doc.resolve_dict(&o).cloned())?;
        let char_procs = procs
            .iter()
            .filter_map(|(name, value)| match doc.resolve_value(value) {
                PdfObject::Stream(stream) => Some((name.clone(), stream)),
                _ => None,
            })
            .collect();
        let resources = dict
            .get("Resources")
            .map(|o| doc.resolve_value(o))
            .and_then(|o| doc.resolve_dict(&o).cloned());
        Some(Type3Glyphs {
            font_matrix,
            char_procs,
            resources,
        })
    }

    /// The procedure that draws `code`, found through the encoding's glyph
    /// name — the only way Type3 glyphs are addressed.
    pub fn procedure(&self, code: u32, text: &TextFont) -> Option<&PdfStream> {
        let name = text.glyph_names.get(&u8::try_from(code).ok()?)?;
        self.char_procs.get(name)
    }
}

/// The embedded outline program, whichever format it turned out to be.
enum Program {
    TrueType(Box<TrueTypeFont>),
    Cff(Box<CffFont>),
    Type1(Box<Type1Font>),
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
    /// Glyph procedures, for a Type3 font.
    type3: Option<Type3Glyphs>,
    /// The font is one of the standard 14 (or a metric-compatible alias such
    /// as Arial), which a document may leave unembedded by design.
    standard14: bool,
}

impl RenderFont {
    pub fn load(doc: &PdfDocument, dict: &Dictionary) -> RenderFont {
        let text = load_font(doc, dict).unwrap_or_default();
        let subtype = dict.get("Subtype").and_then(PdfObject::as_name);

        // Type3 glyphs are content streams. Nothing about them resembles an
        // outline program, so none of the descriptor logic below applies —
        // and a substitute face would be wrong, since the glyphs are right
        // there in the document.
        if subtype == Some("Type3") {
            let type3 = Type3Glyphs::load(doc, dict);
            return RenderFont {
                text,
                source: GlyphSource::Type3,
                program: None,
                cid_to_gid: None,
                composite: false,
                symbolic: false,
                fallback: None,
                type3,
                standard14: false,
            };
        }

        let composite = subtype == Some("Type0");

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

        let base_font = owner
            .get("BaseFont")
            .and_then(PdfObject::as_name)
            .or_else(|| dict.get("BaseFont").and_then(PdfObject::as_name))
            .unwrap_or("");
        let standard14 = !composite && pdf_text::standard14::lookup(base_font).is_some();

        // Nothing drawable was embedded. Rather than skip the text — which
        // renders the page blank and looks like a broken file — borrow a
        // substitute face matched to the font's declared weight, slope and
        // design, and for CJK to the region whose glyph forms it expects.
        let fallback = if program.is_none() {
            let flags = descriptor
                .as_ref()
                .and_then(|d| d.get("Flags"))
                .and_then(PdfObject::as_i64)
                .unwrap_or(0);
            let region = text
                .cid_collection()
                .and_then(CjkRegion::from_collection)
                .or_else(|| CjkRegion::from_font_name(base_font));
            FallbackFont::for_style(FallbackStyle::detect(base_font, flags))
                .map(|fallback| fallback.with_region(region))
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
            type3: None,
            standard14,
        }
    }

    /// Glyph procedures, when this is a Type3 font.
    pub fn type3(&self) -> Option<&Type3Glyphs> {
        self.type3.as_ref()
    }

    /// The em size the outlines from [`RenderFont::outline`] are expressed
    /// in. It must track whichever source actually drew the glyph, or text
    /// scales to the wrong size — Roboto's em is 2048, not 1000.
    pub fn units_per_em(&self) -> f64 {
        match &self.program {
            Some(Program::TrueType(f)) => f.units_per_em,
            Some(Program::Cff(f)) => f.units_per_em,
            Some(Program::Type1(f)) => f.units_per_em,
            None => self
                .fallback
                .as_ref()
                .map(FallbackFont::units_per_em)
                .unwrap_or(1000.0),
        }
    }

    pub fn can_draw_glyphs(&self) -> bool {
        self.program.is_some() || self.fallback.is_some() || self.type3.is_some()
    }

    /// True when the glyphs being drawn are a stand-in rather than the
    /// document's own font, so a caller can note the page is approximate.
    pub fn is_substituted(&self) -> bool {
        self.program.is_none() && self.fallback.is_some()
    }

    /// Substituted, but for a standard 14 font. PDF lets a document leave
    /// those unembedded and every reader supplies its own, with the standard
    /// metrics; telling the reader the page is approximate would be noise.
    pub fn is_standard_substitute(&self) -> bool {
        self.is_substituted() && self.standard14
    }

    /// Advance for one code, in text-space units (em/1000).
    ///
    /// The PDF's own `/Widths` wins whenever it has an entry, so substituted
    /// text still breaks lines where the document intended. Only when the
    /// document supplied nothing — legal for the standard 14 — does the
    /// substitute's own metric fill in, which is far closer than the flat
    /// 500-unit default it replaces.
    pub fn advance_width(&self, code: u32) -> f64 {
        // Type3 /Widths are in glyph space, which only the font's own matrix
        // relates to text space; a code with no width advances nothing.
        if let Some(type3) = &self.type3 {
            let width = self.text.explicit_width(code).unwrap_or(0.0);
            return type3.font_matrix.apply_vector(width, 0.0).0 * 1000.0;
        }
        if let Some(width) = self.text.explicit_width(code) {
            return width;
        }
        if self.text.authoritative_default_width {
            return self.text.default_width;
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
            // have on the stand-in face. The encoding's own character comes
            // first — its glyph name says `fi` where ToUnicode says "fi".
            let fallback = self.fallback.as_ref()?;
            if let Some(outline) = self
                .text
                .encoding_char(code)
                .and_then(|ch| fallback.outline_for_char(ch))
            {
                return Some(outline);
            }
            let text = self.text.decode_code(code);
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_none() {
                return fallback.outline_for_char(first);
            }
            return self.composed_outline(fallback, &text, code);
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
            Program::Type1(program) => {
                let name = self.named_glyph(
                    code,
                    |name| program.has_glyph(name).then(|| name.to_owned()),
                    |byte| {
                        program
                            .builtin_name(byte)
                            .filter(|name| program.has_glyph(name))
                            .map(str::to_owned)
                    },
                )?;
                program.glyph_outline(&name)
            }
        }
    }

    /// One code that stands for several characters — a ligature, an Indic
    /// conjunct — in a font the document did not embed. Without the original
    /// glyph, the characters are drawn side by side and squeezed into the
    /// advance the document gave the code, so the line keeps its layout and
    /// no letter silently disappears.
    fn composed_outline(&self, fallback: &FallbackFont, text: &str, code: u32) -> Option<Path> {
        let parts: Vec<(Option<Path>, f64)> = text
            .chars()
            .map(|ch| {
                (
                    fallback.outline_for_char(ch),
                    fallback.advance(ch).unwrap_or(0.0),
                )
            })
            .collect();
        let natural: f64 = parts.iter().map(|(_, advance)| advance).sum();
        let target = self.advance_width(code) * fallback.units_per_em() / 1000.0;
        let squeeze = if natural > 0.0 && target > 0.0 {
            (target / natural).min(1.0)
        } else {
            1.0
        };
        let mut out = Path::new();
        let mut x = 0.0;
        for (outline, advance) in parts {
            if let Some(outline) = outline {
                out.extend(&outline.transform(&Matrix::new(squeeze, 0.0, 0.0, 1.0, x, 0.0)));
            }
            x += advance * squeeze;
        }
        (!out.is_empty()).then_some(out)
    }

    /// The glyph a simple font addressed by glyph *name* — Type 1 and CFF —
    /// draws for `code` (ISO 32000-1, 9.6.6.2):
    ///
    /// 1. the `/Differences` name, the document's own word on which glyph;
    /// 2. a base encoding the PDF named outright (`/WinAnsiEncoding`);
    /// 3. the program's built-in encoding, which is the implicit base for an
    ///    embedded font — not StandardEncoding;
    /// 4. the implicit base encoding's character;
    /// 5. ToUnicode, last: it serves extraction and may name a ligature's
    ///    first letter.
    ///
    /// Characters become names through every spelling the Adobe Glyph List
    /// knows, so `é` finds `eacute` and a no-break space finds `space`.
    fn named_glyph<G>(
        &self,
        code: u32,
        by_name: impl Fn(&str) -> Option<G>,
        builtin: impl Fn(u8) -> Option<G>,
    ) -> Option<G> {
        let by_char = |ch: char| -> Option<G> {
            pdf_text::agl::names_for_char(ch)
                .iter()
                .find_map(|n| by_name(n))
        };
        let byte = u8::try_from(code).ok();
        if let Some(name) = byte.and_then(|b| self.text.glyph_names.get(&b)) {
            if let Some(glyph) = by_name(name) {
                return Some(glyph);
            }
            // The same character under another spelling: /Differences says
            // `uni00E9`, the program calls it `eacute`.
            if let Some(glyph) = pdf_text::agl::char_for_name(name).and_then(by_char) {
                return Some(glyph);
            }
        }
        if self.text.explicit_base_encoding {
            if let Some(glyph) = self.text.encoding_char(code).and_then(by_char) {
                return Some(glyph);
            }
        }
        if let Some(glyph) = byte.and_then(&builtin) {
            return Some(glyph);
        }
        if let Some(glyph) = self.text.encoding_char(code).and_then(by_char) {
            return Some(glyph);
        }
        self.text.decode_code(code).chars().next().and_then(by_char)
    }

    /// CFF glyph lookup.
    ///
    /// A CID-keyed CFF carries its own CID → glyph-id charset, which is the
    /// authority for a Type0 descendant; `/CIDToGIDMap` only applies to
    /// CIDFontType2 (TrueType) descendants. Simple fonts go through the
    /// encoding's glyph *name*, which is how CFF addresses glyphs natively.
    fn cff_glyph_id(&self, code: u32, program: &CffFont) -> Option<u16> {
        if self.composite {
            let cid = u16::try_from(self.text.cid(code)).ok()?;
            if let Some(gid) = program.gid_for_cid(cid) {
                return Some(gid);
            }
            return (usize::from(cid) < program.num_glyphs()).then_some(cid);
        }

        // Simple fonts are addressed by glyph *name*. The /Differences name is
        // the authority: it names the glyph the document means, whatever
        // Unicode that glyph happens to look like — legacy Indic fonts call a
        // Telugu letter `exclam`, symbol fonts call a bullet `a1`. Subsetters
        // that discard names leave synthetic ones (`g42`) whose number is the
        // glyph index.
        let num_glyphs = program.num_glyphs();
        let by_name = |name: &str| {
            program
                .gid_for_name(name)
                .or_else(|| gid_from_glyph_name(name, num_glyphs))
        };
        if let Some(gid) =
            self.named_glyph(code, by_name, |byte| program.gid_for_builtin_code(byte))
        {
            return Some(gid);
        }
        // Subset fonts frequently have no usable charset, and are built so the
        // code is already the glyph index.
        if (code as usize) < num_glyphs {
            return u16::try_from(code).ok();
        }
        None
    }

    fn glyph_id(&self, code: u32, program: &TrueTypeFont) -> Option<u16> {
        if self.composite {
            // The CMap turns the code into a CID; /CIDToGIDMap (or identity)
            // turns the CID into a glyph.
            let cid = self.text.cid(code);
            return match &self.cid_to_gid {
                Some(table) => table.get(cid as usize).copied(),
                None => u16::try_from(cid).ok(),
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
        // The encoding's character (base encoding + /Differences) is what the
        // spec looks up in the cmap (ISO 32000-1, 9.6.6.4).
        if let Some(ch) = self.text.encoding_char(code) {
            if let Some(gid) = program.glyph_for_char(ch as u32) {
                return Some(gid);
            }
        }
        // A /Differences name the cmap cannot express — a borrowed Latin name
        // on an Indic glyph, or a name with no Unicode at all — still reaches
        // its glyph through the font's own `post` names.
        if let Ok(byte) = u8::try_from(code) {
            if let Some(name) = self.text.glyph_names.get(&byte) {
                if let Some(gid) = program.gid_for_name(name) {
                    return Some(gid);
                }
                if let Some(gid) = gid_from_glyph_name(name, usize::from(program.num_glyphs())) {
                    return Some(gid);
                }
            }
        }
        // ToUnicode is for extraction, and can name several characters for
        // one glyph; it is only a last resort for picking one.
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

/// Glyph id for the synthetic names subsetters emit when they have thrown the
/// real ones away — `g42`, `glyph42`, `index42`, `cid42`, `G42` — where the
/// number *is* the glyph index.
fn gid_from_glyph_name(name: &str, num_glyphs: usize) -> Option<u16> {
    let digits = ["glyph", "index", "cid", "g", "G"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let gid: usize = digits.parse().ok()?;
    (gid < num_glyphs).then_some(gid as u16)
}

/// Parse whichever font program the descriptor embeds.
///
/// `/FontFile` is Type 1, `/FontFile2` TrueType and `/FontFile3` CFF, except
/// that a `/Subtype /OpenType` program can be either — an sfnt wrapper
/// holding `glyf` *or* `CFF `. So the bytes are tried as TrueType first and
/// fall through to CFF, which knows how to unwrap an OpenType container.
/// Writers mislabel programs often enough that a format which fails to parse
/// is retried as the others before giving up.
fn load_program(doc: &PdfDocument, descriptor: &Dictionary) -> (Option<Program>, GlyphSource) {
    let Some(embedded) = font_program(doc, descriptor) else {
        return (None, GlyphSource::NotEmbedded);
    };
    let EmbeddedProgram {
        data,
        prefer_truetype,
        type1,
        length1,
    } = embedded;

    if type1 {
        if let Some(font) = Type1Font::parse(&data, length1) {
            return (Some(Program::Type1(Box::new(font))), GlyphSource::Type1);
        }
    }
    if prefer_truetype {
        if let Some(font) = TrueTypeFont::parse(data.clone()) {
            if font.has_glyf() {
                return (
                    Some(Program::TrueType(Box::new(font))),
                    GlyphSource::TrueType,
                );
            }
        }
    }
    if let Some(font) = CffFont::parse(data.clone()) {
        if font.num_glyphs() > 0 {
            return (Some(Program::Cff(Box::new(font))), GlyphSource::Cff);
        }
    }
    if !prefer_truetype {
        if let Some(font) = TrueTypeFont::parse(data.clone()) {
            if font.has_glyf() {
                return (
                    Some(Program::TrueType(Box::new(font))),
                    GlyphSource::TrueType,
                );
            }
        }
    }
    // A Type 1 program filed under another key.
    if !type1 {
        if let Some(font) = Type1Font::parse(&data, None) {
            return (Some(Program::Type1(Box::new(font))), GlyphSource::Type1);
        }
    }
    (None, GlyphSource::UnsupportedProgram)
}

/// An embedded program and what its descriptor says it is.
struct EmbeddedProgram {
    data: Vec<u8>,
    /// `/FontFile2`, or `/FontFile3` with `/Subtype /OpenType`.
    prefer_truetype: bool,
    /// `/FontFile`: a Type 1 program.
    type1: bool,
    /// `/Length1`: the size of a Type 1 program's cleartext part.
    length1: Option<usize>,
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

/// The embedded font program, if the descriptor has one.
fn font_program(doc: &PdfDocument, descriptor: &Dictionary) -> Option<EmbeddedProgram> {
    for (key, is_truetype) in [
        ("FontFile2", true),
        ("FontFile3", false),
        ("FontFile", false),
    ] {
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
            let length1 = stream
                .dictionary
                .get("Length1")
                .map(|v| doc.resolve_value(v))
                .and_then(|v| v.as_i64())
                .and_then(|v| usize::try_from(v).ok());
            if let Ok(data) = doc.stream_data(&stream) {
                return Some(EmbeddedProgram {
                    data,
                    prefer_truetype: is_truetype || subtype == "OpenType",
                    type1: key == "FontFile",
                    length1,
                });
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
    fn synthetic_subset_names_resolve_to_their_glyph_index() {
        assert_eq!(gid_from_glyph_name("g42", 100), Some(42));
        assert_eq!(gid_from_glyph_name("glyph7", 100), Some(7));
        assert_eq!(gid_from_glyph_name("index0", 100), Some(0));
        assert_eq!(gid_from_glyph_name("cid13", 100), Some(13));
        assert_eq!(gid_from_glyph_name("G9", 100), Some(9));
        // Out of range, and names that merely start with a prefix letter.
        assert_eq!(gid_from_glyph_name("g420", 100), None);
        assert_eq!(gid_from_glyph_name("gamma", 100), None);
        assert_eq!(gid_from_glyph_name("g", 100), None);
        assert_eq!(gid_from_glyph_name("exclam", 100), None);
    }

    #[test]
    fn uni_names_decode_to_their_character() {
        assert_eq!(cff::char_for_uni_name("uni0C15"), Some('\u{0C15}'));
        assert_eq!(cff::char_for_uni_name("u1F600"), Some('\u{1F600}'));
        assert_eq!(cff::char_for_uni_name("exclam"), None);
        assert_eq!(cff::char_for_uni_name("uni0C"), None);
    }

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
        assert_eq!(
            font.units_per_em(),
            font.fallback.as_ref().unwrap().units_per_em()
        );
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

    #[test]
    fn cid_default_width_wins_over_substitute_metrics() {
        let doc = PdfDocument::new_empty("2.0");
        let cid = Dictionary::from([("DW".into(), PdfObject::Real(325.5))]);
        let dict = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Type0".into())),
            ("Encoding".into(), PdfObject::Name("Identity-H".into())),
            (
                "DescendantFonts".into(),
                PdfObject::Array(vec![PdfObject::Dictionary(cid)]),
            ),
        ]);
        let mut font = RenderFont::load(&doc, &dict);
        font.text.to_unicode.insert(1, "A".into());
        assert!(font.is_substituted());
        assert_eq!(font.advance_width(1), 325.5);
    }

    #[test]
    fn missing_width_wins_over_substitute_metrics() {
        let doc = PdfDocument::new_empty("1.7");
        let dict = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Type1".into())),
            ("BaseFont".into(), PdfObject::Name("Helvetica".into())),
            (
                "FontDescriptor".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "MissingWidth".into(),
                    PdfObject::Integer(123),
                )])),
            ),
        ]);
        let font = RenderFont::load(&doc, &dict);
        assert_eq!(font.advance_width(65), 123.0);
    }
}
