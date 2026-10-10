//! Milestone 8 (part 3): font decoding — enough to turn show-text operands
//! into Unicode for the overwhelmingly common cases:
//!
//! * simple fonts with Standard/WinAnsi/MacRoman encodings (+ /Differences)
//! * ToUnicode CMaps (bfchar/bfrange)
//! * Type0/CID fonts with Identity-H + ToUnicode
//! * /Widths and CID /W arrays for advance computation

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use pdf_core::document::PdfDocument;
use pdf_core::error::Result;
use pdf_core::object::{Dictionary, PdfObject};

use crate::cmap::{self, CMap, UnicodeForm};

#[derive(Debug, Clone, Default)]
pub struct Font {
    /// For a composite (Type0) font, the CMap that splits strings into codes
    /// and maps each code to a CID. `None` for simple fonts, whose codes are
    /// single bytes.
    pub cmap: Option<Arc<CMap>>,
    /// The CMap is a predefined Unicode-keyed one, so codes spell text.
    unicode_form: Option<UnicodeForm>,
    /// `Registry-Ordering` of a composite font's character collection, e.g.
    /// `Adobe-Japan1`, for CID → Unicode when there is no ToUnicode.
    cid_collection: Option<String>,
    cid_unicode: OnceLock<Option<Arc<HashMap<u32, String>>>>,
    /// Vertical writing (WMode 1): glyphs advance down the page.
    pub vertical: bool,
    /// Per-CID vertical metrics from `/W2`: `(w1y, vx, vy)` in 1000ths of an
    /// em, w1y being the (usually negative) vertical advance.
    pub vertical_metrics: HashMap<u32, (f64, f64, f64)>,
    /// `/DW2`: default `(vy, w1y)` for vertical writing.
    pub default_vertical: (f64, f64),
    /// code -> unicode string, from the ToUnicode CMap.
    pub to_unicode: HashMap<u32, String>,
    /// code -> char for simple fonts (base encoding + /Differences).
    pub encoding: HashMap<u8, char>,
    /// code -> glyph *name*, exactly as `/Differences` spelled it.
    ///
    /// For a simple font the name — not the Unicode character — is what
    /// addresses a glyph in the embedded program, and the two are not
    /// interchangeable. Legacy Indic, symbol and math fonts routinely give
    /// Telugu or mathematical glyphs ordinary Latin names like `exclam` or
    /// `infinity`, so deriving a name back from [`Font::decode_code`] picks
    /// the wrong glyph or none at all. Extraction still uses `encoding`;
    /// rendering needs this.
    pub glyph_names: HashMap<u8, String>,
    /// The PDF named a base encoding outright — `/Encoding /WinAnsiEncoding`
    /// or a `/BaseEncoding` entry. Otherwise the base is implicit, and for an
    /// embedded font that means the font program's own built-in encoding,
    /// which `encoding` (filled from StandardEncoding) cannot represent.
    pub explicit_base_encoding: bool,
    /// code -> advance width (in 1000ths of an em).
    pub widths: HashMap<u32, f64>,
    /// Default width for codes missing from `widths`.
    pub default_width: f64,
    /// The default comes from the PDF (/MissingWidth or CID /DW, including
    /// the CID default of 1000), so a substitute font must not override it.
    pub authoritative_default_width: bool,
}

impl Font {
    /// True for a composite (Type0) font.
    pub fn is_composite(&self) -> bool {
        self.cmap.is_some()
    }

    /// A composite font's character collection, `Registry-Ordering` — e.g.
    /// `Adobe-Japan1` — which says whose glyph forms the text expects.
    pub fn cid_collection(&self) -> Option<&str> {
        self.cid_collection.as_deref()
    }

    /// Split a string operand into character codes — one byte each for a
    /// simple font, and as the CMap's codespace dictates (one to four bytes,
    /// possibly mixed) for a composite one.
    pub fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        let Some(cmap) = &self.cmap else {
            return bytes.iter().map(|&b| u32::from(b)).collect();
        };
        let mut out = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut rest = bytes;
        while !rest.is_empty() {
            let (code, length) = cmap.next_code(rest);
            out.push(code);
            rest = &rest[length.clamp(1, rest.len())..];
        }
        out
    }

    /// The CID a composite font's code selects; a simple font's code is
    /// returned unchanged.
    pub fn cid(&self, code: u32) -> u32 {
        match &self.cmap {
            Some(cmap) => cmap.cid_of(code),
            None => code,
        }
    }

    /// The character the font's *encoding* (base encoding plus `/Differences`)
    /// assigns to a single-byte code, ignoring `/ToUnicode`.
    ///
    /// Glyph selection must use this, not [`Font::decode_code`]: ToUnicode
    /// exists for text extraction and freely maps one code to several
    /// characters — a ligature to `"fi"`, an Indic conjunct to three code
    /// points — whose first character names the wrong glyph.
    pub fn encoding_char(&self, code: u32) -> Option<char> {
        if self.is_composite() {
            return None;
        }
        self.encoding.get(&u8::try_from(code).ok()?).copied()
    }

    /// Best-effort Unicode for one code.
    pub fn decode_code(&self, code: u32) -> String {
        if let Some(s) = self.to_unicode.get(&code) {
            return s.clone();
        }
        if self.is_composite() {
            // A Unicode-keyed CMap spells the text in its codes.
            if let Some(form) = self.unicode_form {
                let length = ((32 - code.leading_zeros() as usize).div_ceil(8)).max(1);
                if let Some(text) = form.decode(code, length) {
                    return text;
                }
            }
            // Otherwise the CID names a glyph in a known character
            // collection, and Adobe publishes what each one means.
            if let Some(table) = self.cid_unicode_table() {
                if let Some(text) = table.get(&self.cid(code)) {
                    return text.clone();
                }
            }
            return String::new();
        }
        // A /Differences name can stand for several characters — `f_f_i`,
        // `T_h` — which the one-character encoding table cannot hold.
        if let Some(text) = self
            .glyph_names
            .get(&(code as u8))
            .and_then(|name| crate::agl::text_for_name(name))
        {
            return text;
        }
        if let Some(&c) = self.encoding.get(&(code as u8)) {
            return c.to_string();
        }
        // Latin-1 fallback for the printable range.
        if (0x20..=0xFF).contains(&code) {
            if let Some(c) = char::from_u32(code) {
                return c.to_string();
            }
        }
        String::new()
    }

    fn cid_unicode_table(&self) -> Option<&Arc<HashMap<u32, String>>> {
        self.cid_unicode
            .get_or_init(|| {
                self.cid_collection
                    .as_deref()
                    .and_then(cmap::cid_to_unicode)
            })
            .as_ref()
    }

    /// Composite fonts key `/W` by CID, simple fonts `/Widths` by code.
    fn width_key(&self, code: u32) -> u32 {
        self.cid(code)
    }

    /// Advance width for one code, in text-space units (em/1000).
    pub fn width(&self, code: u32) -> f64 {
        self.widths
            .get(&self.width_key(code))
            .copied()
            .unwrap_or(self.default_width)
    }

    /// The width the document itself gave for this code, if it gave one.
    ///
    /// Distinguishes "the PDF says this glyph is 722 wide" from "the PDF said
    /// nothing and 500 is a guess", which is the difference between honouring
    /// a document's layout and inventing one.
    pub fn explicit_width(&self, code: u32) -> Option<f64> {
        self.widths.get(&self.width_key(code)).copied()
    }

    /// Vertical metrics for one code in vertical writing: `(w1y, vx, vy)`,
    /// where `(vx, vy)` is the position vector from the horizontal origin to
    /// the vertical one. Unlisted CIDs use `/DW2` and half the glyph's width.
    pub fn vertical_metrics(&self, code: u32) -> (f64, f64, f64) {
        if let Some(&metrics) = self.vertical_metrics.get(&self.cid(code)) {
            return metrics;
        }
        let (vy, w1y) = self.default_vertical;
        (w1y, self.width(code) / 2.0, vy)
    }

    /// Whether a code is the single-byte space that word spacing applies to.
    /// In a composite font only a code the CMap defines as one byte counts.
    pub fn is_space_code(&self, code: u32) -> bool {
        code == 32
            && match &self.cmap {
                Some(cmap) => cmap.is_code_of_length(32, 1),
                None => true,
            }
    }
}

/// Build a `Font` from a font dictionary.
pub fn load_font(doc: &PdfDocument, dict: &Dictionary) -> Result<Font> {
    let subtype = dict
        .get("Subtype")
        .and_then(PdfObject::as_name)
        .unwrap_or("");
    let mut font = Font {
        default_width: 500.0,
        default_vertical: (880.0, -1000.0),
        ..Default::default()
    };

    if subtype == "Type0" {
        let encoding = dict.get("Encoding").map(|e| doc.resolve_value(e));
        let cmap = match &encoding {
            Some(PdfObject::Name(name)) => {
                font.unicode_form = cmap::is_unicode_keyed(name);
                cmap::predefined(name)
            }
            Some(PdfObject::Stream(_)) => encoding.as_ref().and_then(|e| embedded_cmap(doc, e, 0)),
            _ => None,
        };
        // Identity-H is the default and the only safe guess when a CMap is
        // missing or names one this build does not have.
        let cmap =
            cmap.unwrap_or_else(|| cmap::predefined("Identity-H").expect("Identity-H is built in"));
        font.vertical = cmap.vertical;
        font.cmap = Some(cmap);
        font.default_width = 1000.0;
        font.authoritative_default_width = true;
        // Descendant CIDFont carries /W, /DW, /W2, /DW2 and its collection.
        if let Some(PdfObject::Array(desc)) =
            dict.get("DescendantFonts").map(|d| doc.resolve_value(d))
        {
            if let Some(cid_dict) = desc.first().and_then(|d| doc.resolve_dict(d)) {
                if let Some(dw) = cid_dict
                    .get("DW")
                    .map(|value| doc.resolve_value(value))
                    .as_ref()
                    .and_then(number)
                {
                    font.default_width = dw;
                }
                if let Some(PdfObject::Array(w)) = cid_dict.get("W").map(|w| doc.resolve_value(w)) {
                    parse_cid_widths(doc, &w, &mut font.widths);
                }
                if let Some(PdfObject::Array(dw2)) =
                    cid_dict.get("DW2").map(|v| doc.resolve_value(v))
                {
                    let values: Vec<f64> = dw2
                        .iter()
                        .filter_map(|v| number(&doc.resolve_value(v)))
                        .collect();
                    if let [vy, w1y] = values.as_slice() {
                        font.default_vertical = (*vy, *w1y);
                    }
                }
                if let Some(PdfObject::Array(w2)) = cid_dict.get("W2").map(|v| doc.resolve_value(v))
                {
                    parse_vertical_metrics(doc, &w2, &mut font.vertical_metrics);
                }
                font.cid_collection = cid_dict
                    .get("CIDSystemInfo")
                    .and_then(|info| doc.resolve_dict(info))
                    .and_then(|info| {
                        let text = |key: &str| match info.get(key).map(|v| doc.resolve_value(v)) {
                            Some(PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes)) => {
                                Some(String::from_utf8_lossy(&bytes).into_owned())
                            }
                            _ => None,
                        };
                        Some(format!("{}-{}", text("Registry")?, text("Ordering")?))
                    });
            }
        }
    } else {
        if let Some(width) = dict
            .get("FontDescriptor")
            .and_then(|value| doc.resolve_dict(value))
            .and_then(|descriptor| descriptor.get("MissingWidth"))
            .map(|value| doc.resolve_value(value))
            .as_ref()
            .and_then(number)
        {
            font.default_width = width;
            font.authoritative_default_width = true;
        }
        // Simple font: base encoding + differences. When the PDF names no
        // base, a standard symbolic font (Symbol, ZapfDingbats) means its own
        // built-in encoding; everything else means StandardEncoding.
        let base_font = dict
            .get("BaseFont")
            .and_then(PdfObject::as_name)
            .unwrap_or("");
        let standard = crate::standard14::lookup(base_font);
        let implicit_base = |font: &mut Font| match standard.filter(|s| s.is_symbolic()) {
            Some(symbolic) => apply_builtin_encoding(symbolic, font),
            None => apply_base_encoding("StandardEncoding", &mut font.encoding),
        };
        match dict.get("Encoding").map(|e| doc.resolve_value(e)) {
            Some(PdfObject::Name(name)) => {
                font.explicit_base_encoding = true;
                apply_base_encoding(&name, &mut font.encoding)
            }
            Some(PdfObject::Dictionary(enc)) => {
                if let Some(base) = enc.get("BaseEncoding").and_then(PdfObject::as_name) {
                    font.explicit_base_encoding = true;
                    apply_base_encoding(base, &mut font.encoding);
                } else {
                    implicit_base(&mut font);
                }
                if let Some(PdfObject::Array(diffs)) = enc.get("Differences") {
                    apply_differences(diffs, &mut font.encoding, &mut font.glyph_names);
                }
            }
            _ => implicit_base(&mut font),
        }
        // /Widths indexed from /FirstChar.
        let first = dict
            .get("FirstChar")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0);
        if let Some(PdfObject::Array(widths)) = dict.get("Widths").map(|w| doc.resolve_value(w)) {
            for (i, w) in widths.iter().enumerate() {
                let Some(value) = number(&doc.resolve_value(w)) else {
                    continue;
                };
                font.widths.insert((first + i as i64).max(0) as u32, value);
            }
        }
        // A standard font may omit /Widths altogether; the reader is meant
        // to know them. Without this its lines were set at a flat 500 units
        // per character. An explicit /MissingWidth is still the document's
        // own word, and wins.
        if font.widths.is_empty() && !font.authoritative_default_width {
            if let Some(standard) = standard {
                apply_standard_widths(standard, &mut font);
            }
        }
    }

    // ToUnicode CMap overrides everything.
    if let Some(PdfObject::Stream(stream)) = dict.get("ToUnicode").map(|t| doc.resolve_value(t)) {
        if let Ok(data) = doc.stream_data(&stream) {
            parse_to_unicode(&data, &mut font.to_unicode);
        }
    }
    Ok(font)
}

/// A CMap embedded as a stream, with its parent — named by the stream's
/// `/UseCMap` entry or a `usecmap` in the program — resolved first.
fn embedded_cmap(doc: &PdfDocument, object: &PdfObject, depth: usize) -> Option<Arc<CMap>> {
    if depth > 4 {
        return None;
    }
    let PdfObject::Stream(stream) = doc.resolve_value(object) else {
        return match doc.resolve_value(object) {
            PdfObject::Name(name) => cmap::predefined(&name),
            _ => None,
        };
    };
    let data = doc.stream_data(&stream).ok()?;
    let parsed = cmap::parse(&data);
    let parent = match stream.dictionary.get("UseCMap") {
        Some(entry) => embedded_cmap(doc, entry, depth + 1),
        None => parsed.parent.as_deref().and_then(cmap::predefined),
    };
    let vertical_by_dict = stream
        .dictionary
        .get("WMode")
        .and_then(PdfObject::as_i64)
        .map(|mode| mode == 1);
    let mut built = parsed.into_cmap(parent);
    if let Some(vertical) = vertical_by_dict {
        built.vertical = vertical;
    }
    Some(Arc::new(built))
}

/// A standard symbolic font's own encoding, as characters and glyph names.
fn apply_builtin_encoding(standard: &crate::standard14::StandardFont, font: &mut Font) {
    for code in 0u8..=255 {
        if let Some(name) = standard.builtin_name(code) {
            if let Some(ch) = glyph_to_char(name) {
                font.encoding.insert(code, ch);
            }
            font.glyph_names.insert(code, name.to_owned());
        }
    }
}

/// Widths for every code from a standard font's metrics, through the glyph
/// name each code's encoding gives it.
fn apply_standard_widths(standard: &crate::standard14::StandardFont, font: &mut Font) {
    for code in 0u8..=255 {
        let named = font
            .glyph_names
            .get(&code)
            .and_then(|name| standard.width(name));
        let width = named.or_else(|| {
            let ch = *font.encoding.get(&code)?;
            crate::agl::names_for_char(ch)
                .iter()
                .find_map(|name| standard.width(name))
        });
        if let Some(width) = width {
            font.widths.insert(u32::from(code), width);
        }
    }
}

/// CID /W2 array: [ c [w1y vx vy w1y vx vy …] ] or [ c1 c2 w1y vx vy ].
fn parse_vertical_metrics(
    doc: &PdfDocument,
    items: &[PdfObject],
    out: &mut HashMap<u32, (f64, f64, f64)>,
) {
    let mut i = 0;
    while i < items.len() {
        let Some(first) = doc.resolve_value(&items[i]).as_i64() else {
            i += 1;
            continue;
        };
        match items.get(i + 1).map(|value| doc.resolve_value(value)) {
            Some(PdfObject::Array(values)) => {
                let numbers: Vec<f64> = values
                    .iter()
                    .filter_map(|v| number(&doc.resolve_value(v)))
                    .collect();
                for (offset, triple) in numbers.chunks_exact(3).take(65536).enumerate() {
                    let cid = first + offset as i64;
                    if (0..=65535).contains(&cid) {
                        out.insert(cid as u32, (triple[0], triple[1], triple[2]));
                    }
                }
                i += 2;
            }
            Some(second) => {
                let Some(last) = second.as_i64() else {
                    i += 2;
                    continue;
                };
                let values: Vec<f64> = (2..5)
                    .filter_map(|k| items.get(i + k))
                    .filter_map(|v| number(&doc.resolve_value(v)))
                    .collect();
                if let [w1y, vx, vy] = values.as_slice() {
                    for cid in first.max(0)..=last.min(65535) {
                        out.insert(cid as u32, (*w1y, *vx, *vy));
                    }
                }
                i += 5;
            }
            None => break,
        }
    }
}

/// CID /W array: [ c [w1 w2 …] ] or [ c1 c2 w ].
fn parse_cid_widths(doc: &PdfDocument, items: &[PdfObject], out: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < items.len() {
        let Some(first) = doc.resolve_value(&items[i]).as_i64() else {
            i += 1;
            continue;
        };
        // A /W array can contain an indirect width array (e.g. [0 9 0 R]).
        // Treating that reference as a range endpoint discards every width
        // and makes a proportional font advance by /DW for every glyph.
        match items.get(i + 1).map(|value| doc.resolve_value(value)) {
            Some(PdfObject::Array(widths)) => {
                for (offset, w) in widths.iter().take(65536).enumerate() {
                    let Some(code) = first.checked_add(offset as i64) else {
                        break;
                    };
                    if (0..=65535).contains(&code) {
                        if let Some(value) = number(&doc.resolve_value(w)) {
                            out.insert(code as u32, value);
                        }
                    }
                }
                i += 2;
            }
            Some(second) => {
                let Some(last) = second.as_i64() else {
                    i += 2;
                    continue;
                };
                let Some(value) = items
                    .get(i + 2)
                    .map(|value| doc.resolve_value(value))
                    .as_ref()
                    .and_then(number)
                else {
                    i += 3;
                    continue;
                };
                // CIDs are 16-bit; malformed ranges must not cause an
                // unbounded allocation or wrap large codes onto valid ones.
                for code in first.max(0)..=last.min(65535) {
                    out.insert(code as u32, value);
                }
                i += 3;
            }
            None => break,
        }
    }
}

fn number(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(v) => Some(*v as f64),
        PdfObject::Real(v) if v.is_finite() => Some(*v),
        _ => None,
    }
}

/// Parse bfchar/bfrange sections out of a ToUnicode CMap, including the
/// array form of bfrange that writers use for ligatures and conjuncts.
fn parse_to_unicode(data: &[u8], out: &mut HashMap<u32, String>) {
    out.extend(cmap::parse(data).unicode_table());
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

fn apply_base_encoding(name: &str, out: &mut HashMap<u8, char>) {
    // All three standard Latin text encodings agree with ASCII for 32..=126.
    for code in 0x20..=0x7Eu8 {
        out.insert(code, code as char);
    }
    // WinAnsi is Latin-1 from 0xA0 up — é, ü, ñ, ß — which the table of
    // its 0x80–0x9F differences below does not list.
    if name == "WinAnsiEncoding" {
        for code in 0xA0..=0xFFu8 {
            out.insert(code, code as char);
        }
    }
    let table: &[(u8, char)] = match name {
        "WinAnsiEncoding" => &WIN_ANSI_HIGH,
        "MacRomanEncoding" => &MAC_ROMAN_HIGH,
        _ => &STANDARD_HIGH,
    };
    for &(code, ch) in table {
        out.insert(code, ch);
    }
}

/// Walk a `/Differences` array, recording each code's Unicode character where
/// one can be derived *and* its glyph name always. A name with no Unicode
/// meaning is still the right way to reach the glyph, so it must survive even
/// when `glyph_to_char` gives up.
fn apply_differences(
    diffs: &[PdfObject],
    out: &mut HashMap<u8, char>,
    names: &mut HashMap<u8, String>,
) {
    let mut code: i64 = 0;
    for item in diffs {
        match item {
            PdfObject::Integer(v) => code = *v,
            PdfObject::Real(v) => code = *v as i64,
            PdfObject::Name(glyph) => {
                if (0..=255).contains(&code) {
                    if let Some(ch) = glyph_to_char(glyph) {
                        out.insert(code as u8, ch);
                    }
                    names.insert(code as u8, glyph.clone());
                }
                code += 1;
            }
            _ => {}
        }
    }
}

/// The Unicode character a PostScript glyph name stands for, by the Adobe
/// Glyph List specification (list names, `uniXXXX`, `uXXXX`, ligatures).
pub fn char_for_glyph_name(glyph: &str) -> Option<char> {
    glyph_to_char(glyph)
}

fn glyph_to_char(glyph: &str) -> Option<char> {
    if let Some(ch) = crate::agl::char_for_name(glyph) {
        return Some(ch);
    }
    // Not an AGL name, but writers do name glyphs "1" or "x" and mean it.
    let mut chars = glyph.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c),
        _ => None,
    }
}

/// WinAnsi (cp1252) high range where it differs from Latin-1.
const WIN_ANSI_HIGH: [(u8, char); 27] = [
    (0x80, '\u{20AC}'),
    (0x82, '\u{201A}'),
    (0x83, '\u{0192}'),
    (0x84, '\u{201E}'),
    (0x85, '\u{2026}'),
    (0x86, '\u{2020}'),
    (0x87, '\u{2021}'),
    (0x88, '\u{02C6}'),
    (0x89, '\u{2030}'),
    (0x8A, '\u{0160}'),
    (0x8B, '\u{2039}'),
    (0x8C, '\u{0152}'),
    (0x8E, '\u{017D}'),
    (0x91, '\u{2018}'),
    (0x92, '\u{2019}'),
    (0x93, '\u{201C}'),
    (0x94, '\u{201D}'),
    (0x95, '\u{2022}'),
    (0x96, '\u{2013}'),
    (0x97, '\u{2014}'),
    (0x98, '\u{02DC}'),
    (0x99, '\u{2122}'),
    (0x9A, '\u{0161}'),
    (0x9B, '\u{203A}'),
    (0x9C, '\u{0153}'),
    (0x9E, '\u{017E}'),
    (0x9F, '\u{0178}'),
];

/// MacRoman high range: Mac OS Roman, with 0xDB as the PDF spec's
/// `currency` sign (¤) rather than the later Euro. 0xF0, the Apple logo,
/// has no Unicode meaning and stays unmapped.
const MAC_ROMAN_HIGH: [(u8, char); 127] = [
    (0x80, '\u{00C4}'),
    (0x81, '\u{00C5}'),
    (0x82, '\u{00C7}'),
    (0x83, '\u{00C9}'),
    (0x84, '\u{00D1}'),
    (0x85, '\u{00D6}'),
    (0x86, '\u{00DC}'),
    (0x87, '\u{00E1}'),
    (0x88, '\u{00E0}'),
    (0x89, '\u{00E2}'),
    (0x8A, '\u{00E4}'),
    (0x8B, '\u{00E3}'),
    (0x8C, '\u{00E5}'),
    (0x8D, '\u{00E7}'),
    (0x8E, '\u{00E9}'),
    (0x8F, '\u{00E8}'),
    (0x90, '\u{00EA}'),
    (0x91, '\u{00EB}'),
    (0x92, '\u{00ED}'),
    (0x93, '\u{00EC}'),
    (0x94, '\u{00EE}'),
    (0x95, '\u{00EF}'),
    (0x96, '\u{00F1}'),
    (0x97, '\u{00F3}'),
    (0x98, '\u{00F2}'),
    (0x99, '\u{00F4}'),
    (0x9A, '\u{00F6}'),
    (0x9B, '\u{00F5}'),
    (0x9C, '\u{00FA}'),
    (0x9D, '\u{00F9}'),
    (0x9E, '\u{00FB}'),
    (0x9F, '\u{00FC}'),
    (0xA0, '\u{2020}'),
    (0xA1, '\u{00B0}'),
    (0xA2, '\u{00A2}'),
    (0xA3, '\u{00A3}'),
    (0xA4, '\u{00A7}'),
    (0xA5, '\u{2022}'),
    (0xA6, '\u{00B6}'),
    (0xA7, '\u{00DF}'),
    (0xA8, '\u{00AE}'),
    (0xA9, '\u{00A9}'),
    (0xAA, '\u{2122}'),
    (0xAB, '\u{00B4}'),
    (0xAC, '\u{00A8}'),
    (0xAD, '\u{2260}'),
    (0xAE, '\u{00C6}'),
    (0xAF, '\u{00D8}'),
    (0xB0, '\u{221E}'),
    (0xB1, '\u{00B1}'),
    (0xB2, '\u{2264}'),
    (0xB3, '\u{2265}'),
    (0xB4, '\u{00A5}'),
    (0xB5, '\u{00B5}'),
    (0xB6, '\u{2202}'),
    (0xB7, '\u{2211}'),
    (0xB8, '\u{220F}'),
    (0xB9, '\u{03C0}'),
    (0xBA, '\u{222B}'),
    (0xBB, '\u{00AA}'),
    (0xBC, '\u{00BA}'),
    (0xBD, '\u{03A9}'),
    (0xBE, '\u{00E6}'),
    (0xBF, '\u{00F8}'),
    (0xC0, '\u{00BF}'),
    (0xC1, '\u{00A1}'),
    (0xC2, '\u{00AC}'),
    (0xC3, '\u{221A}'),
    (0xC4, '\u{0192}'),
    (0xC5, '\u{2248}'),
    (0xC6, '\u{2206}'),
    (0xC7, '\u{00AB}'),
    (0xC8, '\u{00BB}'),
    (0xC9, '\u{2026}'),
    (0xCA, '\u{00A0}'),
    (0xCB, '\u{00C0}'),
    (0xCC, '\u{00C3}'),
    (0xCD, '\u{00D5}'),
    (0xCE, '\u{0152}'),
    (0xCF, '\u{0153}'),
    (0xD0, '\u{2013}'),
    (0xD1, '\u{2014}'),
    (0xD2, '\u{201C}'),
    (0xD3, '\u{201D}'),
    (0xD4, '\u{2018}'),
    (0xD5, '\u{2019}'),
    (0xD6, '\u{00F7}'),
    (0xD7, '\u{25CA}'),
    (0xD8, '\u{00FF}'),
    (0xD9, '\u{0178}'),
    (0xDA, '\u{2044}'),
    (0xDB, '\u{00A4}'),
    (0xDC, '\u{2039}'),
    (0xDD, '\u{203A}'),
    (0xDE, '\u{FB01}'),
    (0xDF, '\u{FB02}'),
    (0xE0, '\u{2021}'),
    (0xE1, '\u{00B7}'),
    (0xE2, '\u{201A}'),
    (0xE3, '\u{201E}'),
    (0xE4, '\u{2030}'),
    (0xE5, '\u{00C2}'),
    (0xE6, '\u{00CA}'),
    (0xE7, '\u{00C1}'),
    (0xE8, '\u{00CB}'),
    (0xE9, '\u{00C8}'),
    (0xEA, '\u{00CD}'),
    (0xEB, '\u{00CE}'),
    (0xEC, '\u{00CF}'),
    (0xED, '\u{00CC}'),
    (0xEE, '\u{00D3}'),
    (0xEF, '\u{00D4}'),
    (0xF1, '\u{00D2}'),
    (0xF2, '\u{00DA}'),
    (0xF3, '\u{00DB}'),
    (0xF4, '\u{00D9}'),
    (0xF5, '\u{0131}'),
    (0xF6, '\u{02C6}'),
    (0xF7, '\u{02DC}'),
    (0xF8, '\u{00AF}'),
    (0xF9, '\u{02D8}'),
    (0xFA, '\u{02D9}'),
    (0xFB, '\u{02DA}'),
    (0xFC, '\u{00B8}'),
    (0xFD, '\u{02DD}'),
    (0xFE, '\u{02DB}'),
    (0xFF, '\u{02C7}'),
];

/// Adobe StandardEncoding high range (common subset).
const STANDARD_HIGH: [(u8, char); 12] = [
    (0xA1, '\u{00A1}'),
    (0xA2, '\u{00A2}'),
    (0xA3, '\u{00A3}'),
    (0xB1, '\u{2013}'),
    (0xB4, '\u{00B7}'),
    (0xB7, '\u{2022}'),
    (0xBC, '\u{2026}'),
    (0xD0, '\u{2014}'),
    (0xD1, '\u{2018}'),
    (0xD2, '\u{2019}'),
    (0xAE, '\u{FB01}'),
    (0xAF, '\u{FB02}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_winansi_specials() {
        let mut enc = HashMap::new();
        apply_base_encoding("WinAnsiEncoding", &mut enc);
        assert_eq!(enc.get(&0x93), Some(&'\u{201C}'));
        assert_eq!(enc.get(&0x41), Some(&'A'));
    }

    #[test]
    fn mac_roman_covers_ligatures_and_the_full_high_range() {
        let mut enc = HashMap::new();
        apply_base_encoding("MacRomanEncoding", &mut enc);
        // Core Text writes "fi" and "fl" as MacRoman ligature codes.
        assert_eq!(enc.get(&0xDE), Some(&'\u{FB01}'));
        assert_eq!(enc.get(&0xDF), Some(&'\u{FB02}'));
        assert_eq!(enc.get(&0x8B), Some(&'ã'));
        assert_eq!(enc.get(&0xF5), Some(&'ı'));
        assert_eq!((0x80..=0xFFu8).filter(|c| enc.contains_key(c)).count(), 127);
    }

    #[test]
    fn differences_override_base() {
        let mut enc = HashMap::new();
        apply_base_encoding("WinAnsiEncoding", &mut enc);
        let diffs = vec![
            PdfObject::Integer(65),
            PdfObject::Name("bullet".into()),
            PdfObject::Name("uni0915".into()),
        ];
        let mut names = HashMap::new();
        apply_differences(&diffs, &mut enc, &mut names);
        assert_eq!(enc.get(&65), Some(&'\u{2022}'));
        assert_eq!(enc.get(&66), Some(&'\u{0915}'));
    }

    /// A legacy Telugu font names its glyphs after unrelated Latin and symbol
    /// characters. The names are the only correct handle on those glyphs, so
    /// they must survive whether or not they carry a Unicode meaning.
    #[test]
    fn differences_record_glyph_names_even_without_a_unicode_meaning() {
        let diffs = vec![
            PdfObject::Integer(2),
            PdfObject::Name("greaterequal".into()),
            PdfObject::Name("infinity".into()),
            PdfObject::Integer(33),
            PdfObject::Name("exclam".into()),
            PdfObject::Name("nonesuchglyph".into()),
        ];
        let (mut enc, mut names) = (HashMap::new(), HashMap::new());
        apply_differences(&diffs, &mut enc, &mut names);

        assert_eq!(names.get(&2).map(String::as_str), Some("greaterequal"));
        assert_eq!(names.get(&3).map(String::as_str), Some("infinity"));
        assert_eq!(names.get(&33).map(String::as_str), Some("exclam"));
        // No AGL entry, so no character — but the name is still the way in.
        assert_eq!(enc.get(&34), None);
        assert_eq!(names.get(&34).map(String::as_str), Some("nonesuchglyph"));
    }

    #[test]
    fn parses_tounicode_cmap() {
        let cmap = b"
/CIDInit /ProcSet findresource begin
begincmap
2 beginbfchar
<0041> <0042>
<0042> <00480069>
endbfchar
1 beginbfrange
<0050> <0052> <0061>
endbfrange
endcmap
";
        let mut map = HashMap::new();
        parse_to_unicode(cmap, &mut map);
        assert_eq!(map.get(&0x41).map(String::as_str), Some("B"));
        assert_eq!(map.get(&0x42).map(String::as_str), Some("Hi"));
        assert_eq!(map.get(&0x50).map(String::as_str), Some("a"));
        assert_eq!(map.get(&0x52).map(String::as_str), Some("c"));
    }

    #[test]
    fn cid_width_forms() {
        let mut widths = HashMap::new();
        // [ 1 [500 600] 10 12 250 ]
        let items = vec![
            PdfObject::Integer(1),
            PdfObject::Array(vec![PdfObject::Integer(500), PdfObject::Integer(600)]),
            PdfObject::Integer(10),
            PdfObject::Integer(12),
            PdfObject::Integer(250),
        ];
        parse_cid_widths(&PdfDocument::new_empty("1.7"), &items, &mut widths);
        assert_eq!(widths.get(&1), Some(&500.0));
        assert_eq!(widths.get(&2), Some(&600.0));
        assert_eq!(widths.get(&11), Some(&250.0));
    }

    #[test]
    fn cid_width_array_may_be_indirect() {
        let mut doc = PdfDocument::new_empty("1.7");
        let widths = doc.add_object(PdfObject::Array(vec![
            PdfObject::Integer(250),
            PdfObject::Integer(566),
            PdfObject::Real(555.5),
        ]));
        let cid = Dictionary::from([
            ("DW".into(), PdfObject::Integer(1000)),
            (
                "W".into(),
                PdfObject::Array(vec![
                    PdfObject::Integer(0),
                    PdfObject::Reference(widths),
                    PdfObject::Integer(10),
                    PdfObject::Integer(12),
                    PdfObject::Integer(700),
                ]),
            ),
        ]);
        let dict = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Type0".into())),
            ("Encoding".into(), PdfObject::Name("Identity-H".into())),
            (
                "DescendantFonts".into(),
                PdfObject::Array(vec![PdfObject::Dictionary(cid)]),
            ),
        ]);
        let font = load_font(&doc, &dict).unwrap();
        assert_eq!(font.width(0), 250.0);
        assert_eq!(font.width(1), 566.0);
        assert_eq!(font.width(2), 555.5);
        assert_eq!(font.width(11), 700.0);
        assert_eq!(font.width(3), 1000.0);
    }

    /// A Type0 font with a predefined CMap and a CID collection, as a
    /// Japanese or Chinese document without embedded fonts declares it.
    fn cjk_font(encoding: &str, ordering: &str, extra: Vec<(&str, PdfObject)>) -> Font {
        let mut cid = Dictionary::from([(
            "CIDSystemInfo".into(),
            PdfObject::Dictionary(Dictionary::from([
                (
                    "Registry".into(),
                    PdfObject::LiteralString(b"Adobe".to_vec()),
                ),
                (
                    "Ordering".into(),
                    PdfObject::LiteralString(ordering.as_bytes().to_vec()),
                ),
                ("Supplement".into(), PdfObject::Integer(4)),
            ])),
        )]);
        for (key, value) in extra {
            cid.insert(key.into(), value);
        }
        let dict = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Type0".into())),
            ("Encoding".into(), PdfObject::Name(encoding.into())),
            (
                "DescendantFonts".into(),
                PdfObject::Array(vec![PdfObject::Dictionary(cid)]),
            ),
        ]);
        load_font(&PdfDocument::new_empty("1.7"), &dict).unwrap()
    }

    /// Shift-JIS mixes one-byte Roman with two-byte kanji in one string.
    /// Splitting it two bytes at a time — what Identity-H assumed for every
    /// CMap — turned Japanese documents into nonsense.
    #[test]
    fn shift_jis_strings_split_by_the_predefined_codespace() {
        let font = cjk_font("90ms-RKSJ-H", "Japan1", vec![]);
        // 日本 A 語
        let codes = font.codes(&[0x93, 0xFA, 0x96, 0x7B, 0x41, 0x8C, 0xEA]);
        assert_eq!(codes, vec![0x93FA, 0x967B, 0x41, 0x8CEA]);
        let text: String = codes.iter().map(|&c| font.decode_code(c)).collect();
        assert_eq!(text, "日本A語");
    }

    #[test]
    fn unicode_keyed_cmaps_read_text_straight_off_the_codes() {
        let font = cjk_font("UniGB-UCS2-H", "GB1", vec![]);
        let codes = font.codes(&[0x4E, 0x2D, 0x65, 0x87]);
        let text: String = codes.iter().map(|&c| font.decode_code(c)).collect();
        assert_eq!(text, "中文");
    }

    /// `/W` is indexed by CID. With a non-identity CMap the code and the CID
    /// differ, and reading widths by code spaced every glyph by /DW.
    #[test]
    fn composite_widths_are_looked_up_by_cid() {
        let probe = cjk_font("90ms-RKSJ-H", "Japan1", vec![]);
        let cid = probe.cid(0x8140);
        assert_ne!(cid, 0x8140);
        let font = cjk_font(
            "90ms-RKSJ-H",
            "Japan1",
            vec![(
                "W",
                PdfObject::Array(vec![
                    PdfObject::Integer(i64::from(cid)),
                    PdfObject::Array(vec![PdfObject::Integer(777)]),
                ]),
            )],
        );
        assert_eq!(font.width(0x8140), 777.0);
    }

    #[test]
    fn vertical_cmaps_use_dw2_unless_w2_says_otherwise() {
        let font = cjk_font(
            "UniJIS-UCS2-V",
            "Japan1",
            vec![(
                "W2",
                PdfObject::Array(vec![
                    PdfObject::Integer(1),
                    PdfObject::Integer(1),
                    PdfObject::Integer(-500),
                    PdfObject::Integer(250),
                    PdfObject::Integer(800),
                ]),
            )],
        );
        assert!(font.vertical);
        let listed = font.codes(&[0x00, 0x20]).remove(0);
        assert_eq!(font.cid(listed), 1, "U+0020 is CID 1 in Adobe-Japan1");
        assert_eq!(font.vertical_metrics(listed), (-500.0, 250.0, 800.0));
        // Anything else: DW2's default [880 -1000] and half the width.
        let other = font.codes(&[0x65, 0xE5]).remove(0);
        assert_eq!(font.vertical_metrics(other), (-1000.0, 500.0, 880.0));
    }

    fn simple_font(base_font: &str, extra: Vec<(&str, PdfObject)>) -> Font {
        let mut dict = Dictionary::from([
            ("Subtype".into(), PdfObject::Name("Type1".into())),
            ("BaseFont".into(), PdfObject::Name(base_font.into())),
        ]);
        for (key, value) in extra {
            dict.insert(key.into(), value);
        }
        load_font(&PdfDocument::new_empty("1.7"), &dict).unwrap()
    }

    /// Symbol's code 0x61 is α. Read through StandardEncoding — what an
    /// unencoded font dictionary used to get — it came out as "a".
    #[test]
    fn standard_symbol_font_uses_its_own_encoding() {
        let font = simple_font("Symbol", vec![]);
        let text: String = font
            .codes(b"abp\"")
            .iter()
            .map(|&c| font.decode_code(c))
            .collect();
        assert_eq!(text, "αβπ∀");
        assert_eq!(font.encoding_char(0x61), Some('α'));
    }

    #[test]
    fn standard_dingbats_font_uses_its_own_encoding() {
        let font = simple_font("ZapfDingbats", vec![]);
        assert_eq!(font.decode_code(0x34), "✔");
    }

    /// An explicit base encoding still overrides a symbolic font's own.
    #[test]
    fn an_explicit_encoding_beats_the_builtin_one() {
        let font = simple_font(
            "Symbol",
            vec![("Encoding", PdfObject::Name("WinAnsiEncoding".into()))],
        );
        assert_eq!(font.decode_code(0x61), "a");
    }

    /// A standard font without /Widths is set in its real metrics, not a
    /// flat 500 units per character.
    #[test]
    fn standard_fonts_without_widths_use_their_metrics() {
        let helvetica = simple_font("Helvetica", vec![]);
        assert_eq!(helvetica.width(u32::from(b'A')), 667.0);
        assert_eq!(helvetica.width(u32::from(b'i')), 222.0);
        let courier = simple_font("Courier-Bold", vec![]);
        assert_eq!(courier.width(u32::from(b'i')), 600.0);
        // WinAnsi's é goes through its glyph name to Times' metrics.
        let times = simple_font(
            "Times-Roman",
            vec![("Encoding", PdfObject::Name("WinAnsiEncoding".into()))],
        );
        assert_eq!(times.width(0xE9), 444.0);
        // A document's own /Widths always win.
        let given = simple_font(
            "Helvetica",
            vec![
                ("FirstChar", PdfObject::Integer(65)),
                ("Widths", PdfObject::Array(vec![PdfObject::Integer(999)])),
            ],
        );
        assert_eq!(given.width(65), 999.0);
    }

    /// Word spacing applies to a one-byte code 32 only; in a two-byte CMap the
    /// code 0x0020 is not that.
    #[test]
    fn word_spacing_needs_a_genuine_one_byte_space() {
        assert!(cjk_font("90ms-RKSJ-H", "Japan1", vec![]).is_space_code(32));
        assert!(!cjk_font("Identity-H", "Japan1", vec![]).is_space_code(32));
    }
}
