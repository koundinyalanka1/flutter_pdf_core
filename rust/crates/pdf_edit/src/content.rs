//! Writing content streams, appearance forms and the small objects they
//! use.

use std::io::Write as _;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_text::text_state::Matrix;

use crate::model::Color;

/// A content stream being written.
#[derive(Default)]
pub struct Content {
    pub bytes: Vec<u8>,
}

/// A number as content streams write it: at most three decimals.
pub fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let rounded = (v * 1000.0).round() / 1000.0;
    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        return format!("{}", rounded as i64);
    }
    let mut text = format!("{rounded:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.starts_with("-0.") && text[3..].bytes().all(|b| b == b'0') {
        return "0".into();
    }
    text
}

impl Content {
    pub fn new() -> Self {
        Self::default()
    }

    /// Operands then an operator, on one line.
    pub fn op(&mut self, args: &[f64], op: &str) -> &mut Self {
        for a in args {
            self.bytes.extend_from_slice(num(*a).as_bytes());
            self.bytes.push(b' ');
        }
        self.bytes.extend_from_slice(op.as_bytes());
        self.bytes.push(b'\n');
        self
    }

    pub fn raw(&mut self, text: &str) -> &mut Self {
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(b'\n');
        self
    }

    pub fn cm(&mut self, m: &Matrix) -> &mut Self {
        self.op(&[m.a, m.b, m.c, m.d, m.e, m.f], "cm")
    }

    pub fn fill_color(&mut self, c: Color) -> &mut Self {
        self.op(&[c.0, c.1, c.2], "rg")
    }

    pub fn stroke_color(&mut self, c: Color) -> &mut Self {
        self.op(&[c.0, c.1, c.2], "RG")
    }

    pub fn gs(&mut self, name: &str) -> &mut Self {
        self.raw(&format!("/{name} gs"))
    }

    pub fn move_to(&mut self, p: (f64, f64)) -> &mut Self {
        self.op(&[p.0, p.1], "m")
    }

    pub fn line_to(&mut self, p: (f64, f64)) -> &mut Self {
        self.op(&[p.0, p.1], "l")
    }

    pub fn curve_to(&mut self, a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> &mut Self {
        self.op(&[a.0, a.1, b.0, b.1, c.0, c.1], "c")
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> &mut Self {
        self.op(&[x, y, w, h], "re")
    }

    /// An ellipse inside a box, as four Bézier curves.
    pub fn ellipse(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) -> &mut Self {
        const K: f64 = 0.552_284_75;
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let (rx, ry) = ((x1 - x0).abs() / 2.0, (y1 - y0).abs() / 2.0);
        self.move_to((cx + rx, cy));
        self.curve_to((cx + rx, cy + ry * K), (cx + rx * K, cy + ry), (cx, cy + ry));
        self.curve_to((cx - rx * K, cy + ry), (cx - rx, cy + ry * K), (cx - rx, cy));
        self.curve_to((cx - rx, cy - ry * K), (cx - rx * K, cy - ry), (cx, cy - ry));
        self.curve_to((cx + rx * K, cy - ry), (cx + rx, cy - ry * K), (cx + rx, cy));
        self.raw("h")
    }

    /// A smooth line through points: straight segments between the
    /// midpoints, rounded through each point.
    pub fn smooth(&mut self, points: &[(f64, f64)]) -> &mut Self {
        let Some(&first) = points.first() else { return self };
        self.move_to(first);
        if points.len() < 3 {
            for &p in &points[1..] {
                self.line_to(p);
            }
            return self;
        }
        let mid = |a: (f64, f64), b: (f64, f64)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let mut from = first;
        for pair in points[1..].windows(2) {
            let (control, to) = (pair[0], mid(pair[0], pair[1]));
            // The quadratic from → control → to, as a cubic.
            let c1 = (from.0 + 2.0 / 3.0 * (control.0 - from.0), from.1 + 2.0 / 3.0 * (control.1 - from.1));
            let c2 = (to.0 + 2.0 / 3.0 * (control.0 - to.0), to.1 + 2.0 / 3.0 * (control.1 - to.1));
            self.curve_to(c1, c2, to);
            from = to;
        }
        self.line_to(*points.last().unwrap())
    }

    /// Strokes as a pen draws them: round ends and joins; a single point
    /// is a dot.
    pub fn strokes(&mut self, strokes: &[Vec<(f64, f64)>], width: f64) -> &mut Self {
        self.op(&[width], "w").raw("1 J 1 j");
        for stroke in strokes {
            match stroke.len() {
                0 => {}
                1 => {
                    let (x, y) = stroke[0];
                    self.move_to((x, y)).line_to((x + 0.01, y)).raw("S");
                }
                _ => {
                    self.smooth(stroke).raw("S");
                }
            }
        }
        self
    }
}

/// A stream compressed with Flate.
pub fn flate_stream(mut dict: Dictionary, data: &[u8]) -> PdfObject {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let compressed = encoder.write_all(data).ok().and_then(|_| encoder.finish().ok());
    match compressed {
        Some(bytes) if bytes.len() < data.len() => {
            dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
            PdfObject::Stream(PdfStream::new(dict, bytes))
        }
        _ => PdfObject::Stream(PdfStream::new(dict, data.to_vec())),
    }
}

pub fn name(n: &str) -> PdfObject {
    PdfObject::Name(n.into())
}

pub fn real(v: f64) -> PdfObject {
    let rounded = (v * 1000.0).round() / 1000.0;
    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        PdfObject::Integer(rounded as i64)
    } else {
        PdfObject::Real(rounded)
    }
}

pub fn numbers(values: &[f64]) -> PdfObject {
    PdfObject::Array(values.iter().map(|v| real(*v)).collect())
}

pub fn color_array(c: Color) -> PdfObject {
    numbers(&[c.0, c.1, c.2])
}

pub fn dict(entries: Vec<(&str, PdfObject)>) -> Dictionary {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// A form XObject: an appearance, or content drawn onto a page.
pub fn form(bbox: [f64; 4], matrix: Option<&Matrix>, resources: Dictionary, content: &Content) -> PdfObject {
    let mut d = dict(vec![
        ("Type", name("XObject")),
        ("Subtype", name("Form")),
        ("FormType", PdfObject::Integer(1)),
        ("BBox", numbers(&bbox)),
        ("Resources", PdfObject::Dictionary(resources)),
    ]);
    if let Some(m) = matrix {
        d.insert("Matrix".into(), numbers(&[m.a, m.b, m.c, m.d, m.e, m.f]));
    }
    flate_stream(d, &content.bytes)
}

/// A graphics state with opacities and a blend mode.
pub fn ext_gstate(stroke_alpha: f64, fill_alpha: f64, blend: Option<&str>) -> PdfObject {
    let mut d = dict(vec![
        ("Type", name("ExtGState")),
        ("CA", real(stroke_alpha.clamp(0.0, 1.0))),
        ("ca", real(fill_alpha.clamp(0.0, 1.0))),
    ]);
    if let Some(b) = blend {
        d.insert("BM".into(), name(b));
    }
    PdfObject::Dictionary(d)
}

/// A text string: PDFDocEncoding when it is plain ASCII, UTF-16 otherwise.
pub fn text_string(s: &str) -> PdfObject {
    if s.chars().all(|c| (' '..='~').contains(&c) || c == '\n' || c == '\r' || c == '\t') {
        return PdfObject::LiteralString(s.as_bytes().to_vec());
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    PdfObject::LiteralString(bytes)
}

/// A text string read back: UTF-16 with its byte-order mark, UTF-8 with
/// its mark, or PDFDocEncoding.
pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    bytes.iter().map(|&b| pdf_doc_char(b)).collect()
}

/// PDFDocEncoding: Latin-1 but for 0x18-0x1F and 0x80-0x9F.
fn pdf_doc_char(b: u8) -> char {
    const LOW: [char; 8] = ['\u{02D8}', '\u{02C7}', '\u{02C6}', '\u{02D9}', '\u{02DD}', '\u{02DB}', '\u{02DA}', '\u{02DC}'];
    const HIGH: [char; 32] = [
        '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}', '\u{2044}', '\u{2039}', '\u{203A}', '\u{2212}', '\u{2030}', '\u{201E}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{201A}', '\u{2122}', '\u{FB01}', '\u{FB02}', '\u{0141}', '\u{0152}', '\u{0160}', '\u{0178}', '\u{017D}', '\u{0131}', '\u{0142}', '\u{0153}', '\u{0161}', '\u{017E}', '\u{FFFD}',
    ];
    match b {
        0x18..=0x1F => LOW[(b - 0x18) as usize],
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        0xA0 => '\u{20AC}',
        _ => b as char,
    }
}

/// Now as a PDF date, in UTC.
pub fn now() -> PdfObject {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days since 1970 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let text = format!(
        "D:{year:04}{month:02}{day:02}{:02}{:02}{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    );
    PdfObject::LiteralString(text.into_bytes())
}

/// A name unique to this save, for annotations this app adds.
pub fn unique_name() -> PdfObject {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PdfObject::LiteralString(format!("{OURS}{nanos:x}-{n}").into_bytes())
}

/// How this app's annotations' names begin.
pub const OURS: &str = "dochelper-";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_short_numbers() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(-0.0001), "0");
        assert_eq!(num(2.5), "2.5");
        assert_eq!(num(1.23456), "1.235");
        assert_eq!(num(-3.1), "-3.1");
    }

    #[test]
    fn text_strings_round_trip() {
        for s in ["Plain", "Ünïcödé", "తెలుగు"] {
            let PdfObject::LiteralString(bytes) = text_string(s) else { panic!() };
            assert_eq!(decode_text(&bytes), s);
        }
    }

    #[test]
    fn dates_are_civil() {
        let PdfObject::LiteralString(bytes) = now() else { panic!() };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("D:20"), "{text}");
        assert_eq!(text.len(), 17);
    }
}
