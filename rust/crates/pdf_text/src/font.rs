//! Milestone 8 (part 3): font decoding — enough to turn show-text operands
//! into Unicode for the overwhelmingly common cases:
//!
//! * simple fonts with Standard/WinAnsi/MacRoman encodings (+ /Differences)
//! * ToUnicode CMaps (bfchar/bfrange)
//! * Type0/CID fonts with Identity-H + ToUnicode
//! * /Widths and CID /W arrays for advance computation

use std::collections::HashMap;

use pdf_core::document::PdfDocument;
use pdf_core::error::Result;
use pdf_core::lexer::{Lexer, Token};
use pdf_core::object::{Dictionary, PdfObject};

#[derive(Debug, Clone, Default)]
pub struct Font {
    /// 2-byte codes (Identity-H Type0 fonts).
    pub two_byte_codes: bool,
    /// code -> unicode string, from the ToUnicode CMap.
    pub to_unicode: HashMap<u32, String>,
    /// code -> char for simple fonts (base encoding + /Differences).
    pub encoding: HashMap<u8, char>,
    /// code -> advance width (in 1000ths of an em).
    pub widths: HashMap<u32, f64>,
    /// Default width for codes missing from `widths`.
    pub default_width: f64,
}

impl Font {
    /// Split a string operand into character codes.
    pub fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        if self.two_byte_codes {
            bytes
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        u32::from(c[0]) << 8 | u32::from(c[1])
                    } else {
                        u32::from(c[0])
                    }
                })
                .collect()
        } else {
            bytes.iter().map(|&b| u32::from(b)).collect()
        }
    }

    /// Best-effort Unicode for one code.
    pub fn decode_code(&self, code: u32) -> String {
        if let Some(s) = self.to_unicode.get(&code) {
            return s.clone();
        }
        if !self.two_byte_codes {
            if let Some(&c) = self.encoding.get(&(code as u8)) {
                return c.to_string();
            }
            // Latin-1 fallback for the printable range.
            if (0x20..=0xFF).contains(&code) {
                if let Some(c) = char::from_u32(code) {
                    return c.to_string();
                }
            }
        }
        String::new()
    }

    /// Advance width for one code, in text-space units (em/1000).
    pub fn width(&self, code: u32) -> f64 {
        self.widths.get(&code).copied().unwrap_or(self.default_width)
    }

    /// Whether a (single-byte) code is an ASCII space — used for Tw.
    pub fn is_space_code(&self, code: u32) -> bool {
        !self.two_byte_codes && code == 32
    }
}

/// Build a `Font` from a font dictionary.
pub fn load_font(doc: &PdfDocument, dict: &Dictionary) -> Result<Font> {
    let subtype = dict.get("Subtype").and_then(PdfObject::as_name).unwrap_or("");
    let mut font = Font {
        default_width: 500.0,
        ..Default::default()
    };

    if subtype == "Type0" {
        font.two_byte_codes = matches!(
            dict.get("Encoding").and_then(PdfObject::as_name),
            Some("Identity-H") | Some("Identity-V") | None
        );
        font.default_width = 1000.0;
        // Descendant CIDFont carries /W and /DW.
        if let Some(PdfObject::Array(desc)) = dict.get("DescendantFonts").map(|d| doc.resolve_value(d))
        {
            if let Some(cid_dict) = desc.first().and_then(|d| doc.resolve_dict(d)) {
                if let Some(dw) = cid_dict.get("DW").and_then(PdfObject::as_i64) {
                    font.default_width = dw as f64;
                }
                if let Some(PdfObject::Array(w)) = cid_dict.get("W").map(|w| doc.resolve_value(w)) {
                    parse_cid_widths(&w, &mut font.widths);
                }
            }
        }
    } else {
        // Simple font: base encoding + differences.
        match dict.get("Encoding").map(|e| doc.resolve_value(e)) {
            Some(PdfObject::Name(name)) => apply_base_encoding(&name, &mut font.encoding),
            Some(PdfObject::Dictionary(enc)) => {
                if let Some(base) = enc.get("BaseEncoding").and_then(PdfObject::as_name) {
                    apply_base_encoding(base, &mut font.encoding);
                } else {
                    apply_base_encoding("StandardEncoding", &mut font.encoding);
                }
                if let Some(PdfObject::Array(diffs)) = enc.get("Differences") {
                    apply_differences(diffs, &mut font.encoding);
                }
            }
            _ => apply_base_encoding("StandardEncoding", &mut font.encoding),
        }
        // /Widths indexed from /FirstChar.
        let first = dict.get("FirstChar").and_then(PdfObject::as_i64).unwrap_or(0);
        if let Some(PdfObject::Array(widths)) = dict.get("Widths").map(|w| doc.resolve_value(w)) {
            for (i, w) in widths.iter().enumerate() {
                let value = match w {
                    PdfObject::Integer(v) => *v as f64,
                    PdfObject::Real(v) => *v,
                    _ => continue,
                };
                font.widths.insert((first + i as i64).max(0) as u32, value);
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

/// CID /W array: [ c [w1 w2 …] ] or [ c1 c2 w ].
fn parse_cid_widths(items: &[PdfObject], out: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < items.len() {
        let Some(first) = items[i].as_i64() else {
            i += 1;
            continue;
        };
        match items.get(i + 1) {
            Some(PdfObject::Array(widths)) => {
                for (offset, w) in widths.iter().enumerate() {
                    if let Some(value) = number(w) {
                        out.insert((first + offset as i64).max(0) as u32, value);
                    }
                }
                i += 2;
            }
            Some(second) => {
                let Some(last) = second.as_i64() else {
                    i += 2;
                    continue;
                };
                let Some(value) = items.get(i + 2).and_then(number) else {
                    i += 3;
                    continue;
                };
                for code in first..=last {
                    out.insert(code.max(0) as u32, value);
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
        PdfObject::Real(v) => Some(*v),
        _ => None,
    }
}

/// Parse bfchar/bfrange sections out of a ToUnicode CMap.
fn parse_to_unicode(data: &[u8], out: &mut HashMap<u32, String>) {
    let mut lexer = Lexer::new(data);
    let mut window: Vec<Token> = Vec::new();
    #[derive(PartialEq)]
    enum Mode {
        None,
        BfChar,
        BfRange,
    }
    let mut mode = Mode::None;

    while let Ok(Some(tok)) = lexer.next_token() {
        match &tok.token {
            Token::Keyword(k) if k == "beginbfchar" => {
                mode = Mode::BfChar;
                window.clear();
            }
            Token::Keyword(k) if k == "endbfchar" => {
                mode = Mode::None;
                window.clear();
            }
            Token::Keyword(k) if k == "beginbfrange" => {
                mode = Mode::BfRange;
                window.clear();
            }
            Token::Keyword(k) if k == "endbfrange" => {
                mode = Mode::None;
                window.clear();
            }
            other => {
                if mode == Mode::None {
                    continue;
                }
                window.push(other.clone());
                match mode {
                    Mode::BfChar => {
                        if window.len() == 2 {
                            if let (Token::HexString(src), Token::HexString(dst)) =
                                (&window[0], &window[1])
                            {
                                out.insert(hex_code(src), utf16_be(dst));
                            }
                            window.clear();
                        }
                    }
                    Mode::BfRange => {
                        if window.len() == 3 {
                            apply_bfrange(&window[0], &window[1], &window[2], out);
                            window.clear();
                        }
                    }
                    Mode::None => {}
                }
            }
        }
    }
}

fn apply_bfrange(lo: &Token, hi: &Token, dst: &Token, out: &mut HashMap<u32, String>) {
    let (Token::HexString(lo), Token::HexString(hi)) = (lo, hi) else {
        return;
    };
    let (lo, hi) = (hex_code(lo), hex_code(hi));
    if hi < lo || hi - lo > 0x10000 {
        return;
    }
    match dst {
        Token::HexString(base) => {
            let base_str = utf16_be(base);
            // Increment the last UTF-16 unit per step.
            let mut units: Vec<u16> = base_str.encode_utf16().collect();
            for code in lo..=hi {
                out.insert(code, String::from_utf16_lossy(&units));
                if let Some(last) = units.last_mut() {
                    *last = last.wrapping_add(1);
                }
            }
        }
        Token::ArrayStart => { /* array form is consumed token-by-token; rare, skipped */ }
        _ => {}
    }
}

fn hex_code(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b))
}

fn utf16_be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

fn apply_base_encoding(name: &str, out: &mut HashMap<u8, char>) {
    // All three standard Latin text encodings agree with ASCII for 32..=126.
    for code in 0x20..=0x7Eu8 {
        out.insert(code, code as char);
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

fn apply_differences(diffs: &[PdfObject], out: &mut HashMap<u8, char>) {
    let mut code: i64 = 0;
    for item in diffs {
        match item {
            PdfObject::Integer(v) => code = *v,
            PdfObject::Name(glyph) => {
                if (0..=255).contains(&code) {
                    if let Some(ch) = glyph_to_char(glyph) {
                        out.insert(code as u8, ch);
                    }
                }
                code += 1;
            }
            _ => {}
        }
    }
}

/// A practical subset of the Adobe Glyph List plus uniXXXX support.
fn glyph_to_char(glyph: &str) -> Option<char> {
    if let Some(hex) = glyph.strip_prefix("uni") {
        if hex.len() >= 4 {
            if let Ok(v) = u32::from_str_radix(&hex[..4], 16) {
                return char::from_u32(v);
            }
        }
    }
    if let Some(hex) = glyph.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return char::from_u32(v);
            }
        }
    }
    // Single-letter glyph names map to themselves (A, b, …).
    let mut chars = glyph.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c);
        }
    }
    AGL_SUBSET
        .iter()
        .find(|(name, _)| *name == glyph)
        .map(|(_, c)| *c)
}

const AGL_SUBSET: [(&str, char); 60] = [
    ("space", ' '),
    ("exclam", '!'),
    ("quotedbl", '"'),
    ("numbersign", '#'),
    ("dollar", '$'),
    ("percent", '%'),
    ("ampersand", '&'),
    ("quotesingle", '\''),
    ("parenleft", '('),
    ("parenright", ')'),
    ("asterisk", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("hyphen", '-'),
    ("period", '.'),
    ("slash", '/'),
    ("zero", '0'),
    ("one", '1'),
    ("two", '2'),
    ("three", '3'),
    ("four", '4'),
    ("five", '5'),
    ("six", '6'),
    ("seven", '7'),
    ("eight", '8'),
    ("nine", '9'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less", '<'),
    ("equal", '='),
    ("greater", '>'),
    ("question", '?'),
    ("at", '@'),
    ("bracketleft", '['),
    ("backslash", '\\'),
    ("bracketright", ']'),
    ("underscore", '_'),
    ("braceleft", '{'),
    ("bar", '|'),
    ("braceright", '}'),
    ("quoteleft", '\u{2018}'),
    ("quoteright", '\u{2019}'),
    ("quotedblleft", '\u{201C}'),
    ("quotedblright", '\u{201D}'),
    ("endash", '\u{2013}'),
    ("emdash", '\u{2014}'),
    ("bullet", '\u{2022}'),
    ("ellipsis", '\u{2026}'),
    ("fi", '\u{FB01}'),
    ("fl", '\u{FB02}'),
    ("dagger", '\u{2020}'),
    ("daggerdbl", '\u{2021}'),
    ("copyright", '\u{00A9}'),
    ("registered", '\u{00AE}'),
    ("trademark", '\u{2122}'),
    ("degree", '\u{00B0}'),
    ("eacute", '\u{00E9}'),
    ("egrave", '\u{00E8}'),
    ("agrave", '\u{00E0}'),
    ("ccedilla", '\u{00E7}'),
];

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

/// MacRoman high range (common subset).
const MAC_ROMAN_HIGH: [(u8, char); 32] = [
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
    (0x8C, '\u{00E5}'),
    (0x8D, '\u{00E7}'),
    (0x8E, '\u{00E9}'),
    (0x8F, '\u{00E8}'),
    (0x90, '\u{00EA}'),
    (0x91, '\u{00EB}'),
    (0x92, '\u{00ED}'),
    (0x96, '\u{00F1}'),
    (0x97, '\u{00F3}'),
    (0x9A, '\u{00F6}'),
    (0x9F, '\u{00FC}'),
    (0xA0, '\u{2020}'),
    (0xA1, '\u{00B0}'),
    (0xA5, '\u{2022}'),
    (0xC7, '\u{00AB}'),
    (0xC8, '\u{00BB}'),
    (0xC9, '\u{2026}'),
    (0xD0, '\u{2013}'),
    (0xD1, '\u{2014}'),
    (0xD2, '\u{201C}'),
    (0xD3, '\u{201D}'),
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
    fn differences_override_base() {
        let mut enc = HashMap::new();
        apply_base_encoding("WinAnsiEncoding", &mut enc);
        let diffs = vec![
            PdfObject::Integer(65),
            PdfObject::Name("bullet".into()),
            PdfObject::Name("uni0915".into()),
        ];
        apply_differences(&diffs, &mut enc);
        assert_eq!(enc.get(&65), Some(&'\u{2022}'));
        assert_eq!(enc.get(&66), Some(&'\u{0915}'));
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
        parse_cid_widths(&items, &mut widths);
        assert_eq!(widths.get(&1), Some(&500.0));
        assert_eq!(widths.get(&2), Some(&600.0));
        assert_eq!(widths.get(&11), Some(&250.0));
    }
}
