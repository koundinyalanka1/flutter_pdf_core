//! Glyph names to Unicode, by the Adobe Glyph List Specification.
//!
//! A simple font's `/Differences` names its glyphs; those names are the only
//! route to the text they stand for when a document has no ToUnicode, and the
//! route to the right cmap entry when a TrueType font is drawn. A 60-name
//! subset used to stand in for the list, so Greek (`alpha`), Cyrillic
//! (`afii10017`), Hebrew, Arabic and most accented Latin names decoded to
//! nothing. The full list (`data/glyphlist.txt`) and the ITC Zapf Dingbats
//! names (`data/zapfdingbats.txt`) ship verbatim, BSD-3-Clause, Adobe.

use std::collections::HashMap;
use std::sync::OnceLock;

static GLYPH_LIST: &str = include_str!("../data/glyphlist.txt");
static ZAPF_DINGBATS: &str = include_str!("../data/zapfdingbats.txt");

/// `name;HEX[ HEX…]` lines, comments skipped, keyed by name.
fn table(source: &'static str) -> HashMap<&'static str, &'static str> {
    source
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(';'))
        .collect()
}

fn glyph_list() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| table(GLYPH_LIST))
}

fn dingbats() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| table(ZAPF_DINGBATS))
}

/// The text a glyph name stands for, or `None` when it maps to nothing.
///
/// Follows the AGL specification: everything from the first period on is a
/// variant suffix and dropped (`a.sc` is `a`); underscores join ligature
/// components (`f_f_i` is "ffi"); each component is a list name, a
/// `uniXXXX…` run of BMP values, or a `uXXXX[XX]` value. Dingbat names
/// (`a1`…`a191`) are tried last so they never shadow a list name.
pub fn text_for_name(name: &str) -> Option<String> {
    let base = name.split('.').next().unwrap_or("");
    if base.is_empty() {
        return None;
    }
    let mut out = String::new();
    for component in base.split('_') {
        out.push_str(&component_text(component)?);
    }
    (!out.is_empty()).then_some(out)
}

/// The first character of [`text_for_name`] — what selecting a single glyph
/// by its Unicode value needs.
pub fn char_for_name(name: &str) -> Option<char> {
    text_for_name(name)?.chars().next()
}

/// Every glyph name a character may be spelled with, the conventional one
/// first, for finding it in a font program addressed by name — a Type 1 or
/// CFF font drawn through a base encoding like WinAnsi, which yields
/// characters, not names. Callers try each until the font has one.
pub fn names_for_char(ch: char) -> Vec<String> {
    static REVERSE: OnceLock<HashMap<char, Vec<&'static str>>> = OnceLock::new();
    let reverse = REVERSE.get_or_init(|| {
        let mut reverse: HashMap<char, Vec<&'static str>> = HashMap::new();
        for (name, hex) in glyph_list().iter().chain(dingbats().iter()) {
            let mut values = hex.split_whitespace();
            if let (Some(value), None) = (values.next(), values.next()) {
                if let Some(ch) = u32::from_str_radix(value, 16).ok().and_then(char::from_u32) {
                    reverse.entry(ch).or_default().push(name);
                }
            }
        }
        // Prefer short, ordinary names (`Aacute` over `afii…` style aliases)
        // and settle ties alphabetically so the order is stable.
        for names in reverse.values_mut() {
            names.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
        }
        reverse
    });
    let mut names = Vec::new();
    // The PDF base encodings call a no-break space "space" and a soft hyphen
    // "hyphen"; fonts built for them only have those glyphs.
    match ch {
        '\u{00A0}' => names.push("space".to_owned()),
        '\u{00AD}' => names.push("hyphen".to_owned()),
        _ => {}
    }
    if let Some(list) = reverse.get(&ch) {
        names.extend(list.iter().map(|n| (*n).to_owned()));
    }
    let value = ch as u32;
    if value <= 0xFFFF {
        names.push(format!("uni{value:04X}"));
    }
    names.push(format!("u{value:04X}"));
    names
}

fn component_text(component: &str) -> Option<String> {
    if let Some(hex) = glyph_list().get(component) {
        return decode_hex_list(hex);
    }
    // uniXXXX, uniXXXXYYYY…: BMP values, four digits each, no surrogates.
    if let Some(digits) = component.strip_prefix("uni") {
        if !digits.is_empty() && digits.len() % 4 == 0 && is_upper_hex(digits) {
            let mut text = String::new();
            for chunk in digits.as_bytes().chunks(4) {
                let value = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
                if (0xD800..=0xDFFF).contains(&value) {
                    return None;
                }
                text.push(char::from_u32(value)?);
            }
            return Some(text);
        }
    }
    // uXXXX to uXXXXXX: any scalar value.
    if let Some(digits) = component.strip_prefix('u') {
        if (4..=6).contains(&digits.len()) && is_upper_hex(digits) {
            let value = u32::from_str_radix(digits, 16).ok()?;
            return char::from_u32(value).map(String::from);
        }
    }
    if let Some(hex) = dingbats().get(component) {
        return decode_hex_list(hex);
    }
    // Writers that ignore the case rule still mean what they say; accept
    // lowercase hex as a last resort rather than lose the character.
    if let Some(digits) = component.strip_prefix("uni") {
        if digits.len() == 4 {
            if let Ok(value) = u32::from_str_radix(digits, 16) {
                return char::from_u32(value).map(String::from);
            }
        }
    }
    None
}

fn is_upper_hex(digits: &str) -> bool {
    digits
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

fn decode_hex_list(hex: &str) -> Option<String> {
    hex.split_whitespace()
        .map(|value| u32::from_str_radix(value, 16).ok().and_then(char::from_u32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_names_cover_more_than_latin() {
        assert_eq!(char_for_name("A"), Some('A'));
        assert_eq!(char_for_name("eacute"), Some('é'));
        assert_eq!(char_for_name("alpha"), Some('α'));
        assert_eq!(char_for_name("afii10017"), Some('А'), "Cyrillic capital A");
        assert_eq!(char_for_name("alef"), Some('א'));
        assert_eq!(char_for_name("summation"), Some('∑'));
        assert_eq!(char_for_name("fi"), Some('\u{FB01}'));
    }

    #[test]
    fn suffixes_are_dropped_and_ligatures_decomposed() {
        assert_eq!(text_for_name("a.sc").as_deref(), Some("a"));
        assert_eq!(text_for_name("f_f_i").as_deref(), Some("ffi"));
        assert_eq!(text_for_name("T_h.liga").as_deref(), Some("Th"));
        assert_eq!(text_for_name(".notdef"), None);
    }

    #[test]
    fn uni_and_u_forms() {
        assert_eq!(text_for_name("uni0C15").as_deref(), Some("క"));
        assert_eq!(text_for_name("uni0C150C4D").as_deref(), Some("క్"));
        assert_eq!(text_for_name("u1F600").as_deref(), Some("\u{1F600}"));
        // Surrogates are not characters.
        assert_eq!(text_for_name("uniD800"), None);
        // Lowercase hex breaks the rule but is still understood.
        assert_eq!(text_for_name("uni0c15").as_deref(), Some("క"));
    }

    #[test]
    fn dingbat_names_reach_the_dingbats_block() {
        assert_eq!(char_for_name("a1"), Some('\u{2701}'));
        assert_eq!(char_for_name("a20"), Some('\u{2714}'), "heavy check mark");
    }

    #[test]
    fn multi_codepoint_list_entries_keep_every_character() {
        // A Hebrew point combination that the list spells as two values.
        let text = text_for_name("dalethatafpatah").unwrap();
        assert_eq!(text.chars().count(), 2);
    }

    #[test]
    fn characters_find_their_conventional_names_first() {
        assert_eq!(names_for_char('é')[0], "eacute");
        assert_eq!(names_for_char('\u{2019}')[0], "quoteright");
        assert_eq!(names_for_char('\u{00A0}')[0], "space");
        assert!(names_for_char('\u{FB01}').contains(&"fi".to_owned()));
        assert!(names_for_char('А').contains(&"afii10017".to_owned()));
        // Characters the list never named still have the uniXXXX spelling.
        assert!(names_for_char('క').contains(&"uni0C15".to_owned()));
    }

    #[test]
    fn unknown_names_map_to_nothing() {
        assert_eq!(text_for_name("nonesuchglyph"), None);
        assert_eq!(text_for_name("g123"), None);
    }
}
