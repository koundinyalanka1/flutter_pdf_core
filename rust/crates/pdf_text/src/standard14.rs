//! The 14 standard fonts — Times, Helvetica, Courier (each in four styles),
//! Symbol and ZapfDingbats — which a PDF may use without embedding and, before
//! PDF 1.5, without even giving widths for.
//!
//! Two things depend on knowing them. A document that omits `/Widths` for
//! Helvetica still expects Helvetica's advances, or its lines are set with
//! the wrong spacing. And Symbol and ZapfDingbats have encodings of their own:
//! code 0x61 in Symbol is α, not "a", and in ZapfDingbats it is ❁.
//!
//! The metrics are Adobe's Core 14 AFM files (`data/afm`, distributed under
//! the terms in `data/afm/MustRead.html`), parsed once when first needed.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One standard font's metrics.
pub struct StandardFont {
    pub name: &'static str,
    /// Glyph name → advance width in 1000ths of an em.
    widths: HashMap<&'static str, f64>,
    /// The built-in encoding: code → glyph name.
    builtin: HashMap<u8, &'static str>,
    pub ascender: f64,
    pub descender: f64,
}

impl StandardFont {
    /// The advance of a glyph, by name.
    pub fn width(&self, glyph: &str) -> Option<f64> {
        self.widths.get(glyph).copied()
    }

    /// The glyph name the font's own encoding gives `code`.
    pub fn builtin_name(&self, code: u8) -> Option<&'static str> {
        self.builtin.get(&code).copied()
    }

    /// Symbol and ZapfDingbats: fonts whose built-in encoding is their own,
    /// not StandardEncoding, and is what an unencoded font dictionary means.
    pub fn is_symbolic(&self) -> bool {
        matches!(self.name, "Symbol" | "ZapfDingbats")
    }
}

const AFMS: [(&str, &str); 14] = [
    ("Courier", include_str!("../data/afm/Courier.afm")),
    ("Courier-Bold", include_str!("../data/afm/Courier-Bold.afm")),
    (
        "Courier-Oblique",
        include_str!("../data/afm/Courier-Oblique.afm"),
    ),
    (
        "Courier-BoldOblique",
        include_str!("../data/afm/Courier-BoldOblique.afm"),
    ),
    ("Helvetica", include_str!("../data/afm/Helvetica.afm")),
    (
        "Helvetica-Bold",
        include_str!("../data/afm/Helvetica-Bold.afm"),
    ),
    (
        "Helvetica-Oblique",
        include_str!("../data/afm/Helvetica-Oblique.afm"),
    ),
    (
        "Helvetica-BoldOblique",
        include_str!("../data/afm/Helvetica-BoldOblique.afm"),
    ),
    ("Times-Roman", include_str!("../data/afm/Times-Roman.afm")),
    ("Times-Bold", include_str!("../data/afm/Times-Bold.afm")),
    ("Times-Italic", include_str!("../data/afm/Times-Italic.afm")),
    (
        "Times-BoldItalic",
        include_str!("../data/afm/Times-BoldItalic.afm"),
    ),
    ("Symbol", include_str!("../data/afm/Symbol.afm")),
    ("ZapfDingbats", include_str!("../data/afm/ZapfDingbats.afm")),
];

/// The standard font a `/BaseFont` name stands for, accepting the subset
/// prefix and the metric-compatible names writers use in its place: Arial
/// for Helvetica, Times New Roman for Times, Courier New for Courier.
pub fn lookup(base_font: &str) -> Option<&'static StandardFont> {
    let canonical = canonical_name(base_font)?;
    fonts().get(canonical)
}

fn canonical_name(base_font: &str) -> Option<&'static str> {
    let name = base_font.rsplit('+').next().unwrap_or(base_font);
    let lower: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    // Narrow and condensed cuts share the family name but not the widths.
    if ["narrow", "condensed", "compressed", "black", "light"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return None;
    }
    if lower.contains("zapfdingbats") || lower == "dingbats" {
        return Some("ZapfDingbats");
    }
    if lower.starts_with("symbol") {
        return Some("Symbol");
    }
    let bold = lower.contains("bold");
    let slanted = lower.contains("italic") || lower.contains("oblique");
    let family = if lower.starts_with("courier") {
        "Courier"
    } else if lower.starts_with("helvetica") || lower.starts_with("arial") {
        "Helvetica"
    } else if lower.starts_with("times") {
        "Times"
    } else {
        return None;
    };
    Some(match (family, bold, slanted) {
        ("Times", false, false) => "Times-Roman",
        ("Times", true, false) => "Times-Bold",
        ("Times", false, true) => "Times-Italic",
        ("Times", true, true) => "Times-BoldItalic",
        ("Courier", false, false) => "Courier",
        ("Courier", true, false) => "Courier-Bold",
        ("Courier", false, true) => "Courier-Oblique",
        ("Courier", true, true) => "Courier-BoldOblique",
        (_, false, false) => "Helvetica",
        (_, true, false) => "Helvetica-Bold",
        (_, false, true) => "Helvetica-Oblique",
        (_, true, true) => "Helvetica-BoldOblique",
    })
}

fn fonts() -> &'static HashMap<&'static str, StandardFont> {
    static FONTS: OnceLock<HashMap<&'static str, StandardFont>> = OnceLock::new();
    FONTS.get_or_init(|| {
        AFMS.iter()
            .map(|&(name, source)| (name, parse_afm(name, source)))
            .collect()
    })
}

/// The `C code ; WX width ; N name ; …` character metrics and the vertical
/// extents — all this module needs from an AFM file.
fn parse_afm(name: &'static str, source: &'static str) -> StandardFont {
    let mut font = StandardFont {
        name,
        widths: HashMap::new(),
        builtin: HashMap::new(),
        ascender: 750.0,
        descender: -250.0,
    };
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("Ascender ") {
            font.ascender = value.trim().parse().unwrap_or(font.ascender);
        } else if let Some(value) = line.strip_prefix("Descender ") {
            font.descender = value.trim().parse().unwrap_or(font.descender);
        } else if line.starts_with("C ") {
            let (mut code, mut width, mut glyph) = (None, None, None);
            for field in line.split(';') {
                let mut parts = field.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some("C"), Some(v)) => code = v.parse::<i32>().ok(),
                    (Some("WX"), Some(v)) => width = v.parse::<f64>().ok(),
                    (Some("N"), Some(v)) => glyph = Some(v),
                    _ => {}
                }
            }
            if let (Some(width), Some(glyph)) = (width, glyph) {
                font.widths.insert(glyph, width);
                if let Some(code) = code.and_then(|c| u8::try_from(c).ok()) {
                    font.builtin.insert(code, glyph);
                }
            }
        }
    }
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_metric_compatible_aliases_resolve() {
        for (given, expected) in [
            ("Helvetica", "Helvetica"),
            ("ABCDEF+Helvetica-BoldOblique", "Helvetica-BoldOblique"),
            ("Arial,Bold", "Helvetica-Bold"),
            ("ArialMT", "Helvetica"),
            ("Arial-ItalicMT", "Helvetica-Oblique"),
            ("Times-Roman", "Times-Roman"),
            ("TimesNewRomanPS-BoldItalicMT", "Times-BoldItalic"),
            ("Times New Roman,Italic", "Times-Italic"),
            ("CourierNewPSMT", "Courier"),
            ("Courier-Bold", "Courier-Bold"),
            ("SymbolMT", "Symbol"),
            ("ZapfDingbats", "ZapfDingbats"),
        ] {
            assert_eq!(lookup(given).map(|f| f.name), Some(expected), "{given}");
        }
        assert!(lookup("ArialNarrow").is_none(), "narrow widths differ");
        assert!(lookup("Garamond").is_none());
    }

    #[test]
    fn widths_come_from_the_afm() {
        let helvetica = lookup("Helvetica").unwrap();
        assert_eq!(helvetica.width("A"), Some(667.0));
        assert_eq!(helvetica.width("space"), Some(278.0));
        let courier = lookup("Courier").unwrap();
        assert_eq!(courier.width("W"), Some(600.0));
        assert_eq!(lookup("Times-Roman").unwrap().width("eacute"), Some(444.0));
    }

    #[test]
    fn symbol_and_dingbats_have_their_own_encodings() {
        let symbol = lookup("Symbol").unwrap();
        assert!(symbol.is_symbolic());
        assert_eq!(symbol.builtin_name(0x61), Some("alpha"));
        assert_eq!(symbol.builtin_name(0x22), Some("universal"));
        let dingbats = lookup("ZapfDingbats").unwrap();
        assert_eq!(dingbats.builtin_name(0x34), Some("a20"), "✔ heavy check");
        assert!(!lookup("Helvetica").unwrap().is_symbolic());
    }
}
