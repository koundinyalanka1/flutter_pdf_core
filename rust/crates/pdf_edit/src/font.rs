//! Text in the standard fonts: WinAnsi codes, advances from Adobe's
//! metrics, and lines broken to a width.

use pdf_core::object::PdfObject;
use pdf_core::writer::PdfWriter;
use pdf_text::agl::names_for_char;
use pdf_text::standard14;

use crate::content::{dict, name, Content};
use crate::model::FontChoice;

/// A font text is set in, by its WinAnsi codes.
#[derive(Clone)]
pub struct TextFont {
    /// The /BaseFont of a standard font.
    pub base: String,
    /// Advance per code, in 1000ths of an em.
    widths: Vec<f64>,
    /// Ascender and descender, 1000ths of an em (descender negative).
    pub ascent: f64,
    pub descent: f64,
}

/// cp1252's codes 0x80-0x9F.
const HIGH: [(u8, char); 27] = [
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

/// A character's WinAnsi code.
pub fn win_ansi_code(ch: char) -> Option<u8> {
    match ch as u32 {
        0x20..=0x7E | 0xA0..=0xFF => Some(ch as u32 as u8),
        _ => HIGH.iter().find(|(_, c)| *c == ch).map(|(b, _)| *b),
    }
}

/// The character a WinAnsi code stands for.
pub fn win_ansi_char(code: u8) -> Option<char> {
    match code {
        0x20..=0x7E | 0xA0..=0xFF => Some(code as char),
        _ => HIGH.iter().find(|(b, _)| *b == code).map(|(_, c)| *c),
    }
}

/// Text in WinAnsi, and whether every character had a code (the others
/// become "?").
pub fn win_ansi(text: &str) -> (Vec<u8>, bool) {
    let mut lossless = true;
    let bytes = text
        .chars()
        .filter(|c| *c != '\r')
        .map(|c| match c {
            '\t' => b' ',
            '\n' => b'\n',
            _ => win_ansi_code(c).unwrap_or_else(|| {
                lossless = false;
                b'?'
            }),
        })
        .collect();
    (bytes, lossless)
}

impl TextFont {
    /// One of the 14 standard fonts, set through WinAnsiEncoding.
    pub fn standard(base: &str) -> TextFont {
        let metrics = standard14::lookup(base).or_else(|| standard14::lookup("Helvetica")).expect("Helvetica's metrics ship with the engine");
        let mut widths = vec![0.0; 256];
        for (code, slot) in widths.iter_mut().enumerate() {
            let Some(ch) = win_ansi_char(code as u8) else { continue };
            *slot = names_for_char(ch).iter().find_map(|n| metrics.width(n)).unwrap_or(0.0);
        }
        TextFont {
            base: metrics.name.to_owned(),
            widths,
            ascent: metrics.ascender,
            descent: metrics.descender.min(0.0),
        }
    }

    /// The family and weight a mark asked for.
    pub fn choice(choice: FontChoice, bold: bool) -> TextFont {
        TextFont::standard(match (choice, bold) {
            (FontChoice::Sans, false) => "Helvetica",
            (FontChoice::Sans, true) => "Helvetica-Bold",
            (FontChoice::Serif, false) => "Times-Roman",
            (FontChoice::Serif, true) => "Times-Bold",
            (FontChoice::Mono, false) => "Courier",
            (FontChoice::Mono, true) => "Courier-Bold",
        })
    }

    /// A font whose advances a document gives (`/FirstChar`, `/Widths`),
    /// with a standard font's for codes it leaves out.
    pub fn with_widths(base: &str, first: usize, given: &[f64], fallback: &TextFont) -> TextFont {
        let mut font = fallback.clone();
        font.base = base.to_owned();
        for (i, w) in given.iter().enumerate() {
            if let Some(slot) = font.widths.get_mut(first + i) {
                if *w > 0.0 {
                    *slot = *w;
                }
            }
        }
        font
    }

    /// The font's dictionary, for a resource.
    pub fn dict(&self) -> PdfObject {
        let mut d = dict(vec![
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name(&self.base)),
        ]);
        if !matches!(self.base.as_str(), "Symbol" | "ZapfDingbats") {
            d.insert("Encoding".into(), name("WinAnsiEncoding"));
        }
        PdfObject::Dictionary(d)
    }

    /// The width of encoded text at a size.
    pub fn width(&self, bytes: &[u8], size: f64) -> f64 {
        bytes.iter().map(|b| self.widths[*b as usize]).sum::<f64>() * size / 1000.0
    }

    /// Encoded text broken into lines no wider than `max` (a word wider
    /// than that is broken between characters).
    pub fn wrap(&self, text: &[u8], size: f64, max: f64) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        for paragraph in text.split(|b| *b == b'\n') {
            let mut line: Vec<u8> = Vec::new();
            for word in paragraph.split(|b| *b == b' ') {
                let joined = if line.is_empty() { word.to_vec() } else { [line.as_slice(), b" ", word].concat() };
                if self.width(&joined, size) <= max || line.is_empty() && word.is_empty() {
                    line = joined;
                    continue;
                }
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                // The word on its own line, broken if it still does not fit.
                for &b in word {
                    if !line.is_empty() && self.width(&[line.as_slice(), &[b]].concat(), size) > max {
                        lines.push(std::mem::take(&mut line));
                    }
                    line.push(b);
                }
            }
            lines.push(line);
        }
        lines
    }
}

/// Encoded text as a string operand.
pub fn literal(bytes: &[u8]) -> String {
    let encoded = PdfWriter::serialize_object(&PdfObject::LiteralString(bytes.to_vec())).unwrap_or_default();
    String::from_utf8_lossy(&encoded).into_owned()
}

/// One line of text with its baseline's start at (x, y).
pub fn show(content: &mut Content, resource: &str, size: f64, x: f64, y: f64, bytes: &[u8]) {
    content.raw("BT");
    content.raw(&format!("/{resource} {} Tf", crate::content::num(size)));
    content.op(&[x, y], "Td");
    content.raw(&format!("{} Tj", literal(bytes)));
    content.raw("ET");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_helvetica() {
        let font = TextFont::standard("Helvetica");
        // "Hello": H 722, e 556, l 222, l 222, o 556.
        assert!((font.width(b"Hello", 10.0) - 22.78).abs() < 1e-9);
        assert_eq!(font.width(&[0x80], 1000.0), 556.0, "the euro sign");
    }

    #[test]
    fn encodes_win_ansi() {
        assert_eq!(win_ansi("Café €5"), (vec![b'C', b'a', b'f', 0xE9, b' ', 0x80, b'5'], true));
        assert!(!win_ansi("తె").1);
    }

    #[test]
    fn wraps_words_and_breaks_long_ones() {
        let font = TextFont::standard("Courier");
        // Courier: 600 per character, so 10 characters at 10 pt are 60 pt.
        let lines = font.wrap(b"one two three\nabcdefghijklmnop", 10.0, 60.0);
        let lines: Vec<String> = lines.iter().map(|l| String::from_utf8(l.clone()).unwrap()).collect();
        assert_eq!(lines, ["one two", "three", "abcdefghij", "klmnop"]);
    }
}
