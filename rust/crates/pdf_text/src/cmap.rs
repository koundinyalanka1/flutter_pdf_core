//! CMaps (ISO 32000-1, 9.7.5): how a composite font's string bytes split into
//! character codes, which CID each code selects, and — for ToUnicode and the
//! Adobe `*-UCS2` tables — which Unicode text a code stands for.
//!
//! Only `Identity-H`/`-V` used to be understood. Every other encoding was
//! read one byte at a time with the byte used as the CID, so any Chinese,
//! Japanese or Korean document using a predefined CMap such as
//! `UniGB-UCS2-H` or `90ms-RKSJ-H` — embedded font or not — came out as
//! unrelated glyphs. The predefined CMaps ship compiled into this crate (see
//! [`predefined`]); a CMap embedded as a stream is parsed by [`parse`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pdf_core::lexer::{Lexer, Token};

/// A code → CID mapping with the codespace that splits bytes into codes.
#[derive(Debug, Default)]
pub struct CMap {
    pub name: String,
    /// WMode 1: glyphs advance downwards.
    pub vertical: bool,
    /// Codespace ranges by code length; index 0 holds one-byte ranges.
    codespace: [Vec<(u32, u32)>; 4],
    /// `(first code, last code, first CID)` by code length: sorted and
    /// non-overlapping, so lookup is a plain binary search.
    ranges: [Vec<(u32, u32, u32)>; 4],
    /// `notdefrange` entries: every code in the range selects the one CID,
    /// but only when no real mapping claims it.
    notdef: [Vec<(u32, u32, u32)>; 4],
    /// The CMap named by `usecmap`, consulted after this one's own ranges.
    parent: Option<Arc<CMap>>,
}

impl CMap {
    /// `Identity-H` / `Identity-V`: two-byte codes, each its own CID.
    pub fn identity(vertical: bool) -> CMap {
        let mut cmap = CMap {
            name: if vertical { "Identity-V" } else { "Identity-H" }.to_owned(),
            vertical,
            ..CMap::default()
        };
        cmap.codespace[1].push((0, 0xFFFF));
        cmap.ranges[1].push((0, 0xFFFF, 0));
        cmap
    }

    /// Split the next code off the front of `bytes`: `(code, length)`.
    ///
    /// A code is complete as soon as its first `n` bytes fall in an `n`-byte
    /// codespace range (9.7.6.2). Bytes that match no range are consumed one
    /// at a time, so a damaged string still advances instead of stalling.
    pub fn next_code(&self, bytes: &[u8]) -> (u32, usize) {
        let mut code = 0u32;
        for (n, &byte) in bytes.iter().take(4).enumerate() {
            code = (code << 8) | u32::from(byte);
            if self.in_codespace(n, code) {
                return (code, n + 1);
            }
        }
        (bytes.first().copied().map(u32::from).unwrap_or(0), 1)
    }

    fn in_codespace(&self, index: usize, code: u32) -> bool {
        self.codespace[index]
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&code))
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.in_codespace(index, code))
    }

    fn has_codespace(&self) -> bool {
        self.codespace.iter().any(|c| !c.is_empty())
            || self.parent.as_ref().is_some_and(|p| p.has_codespace())
    }

    /// Whether `code` (of `length` bytes) lies in a codespace range — the
    /// word-spacing rule needs to know a 32 is a genuine one-byte code.
    pub fn is_code_of_length(&self, code: u32, length: usize) -> bool {
        (1..=4).contains(&length) && self.in_codespace(length - 1, code)
    }

    /// The CID a code selects. Unmapped codes select CID 0, the `.notdef`
    /// glyph, as the spec requires.
    pub fn cid(&self, code: u32, length: usize) -> u32 {
        self.mapped(code, length)
            .or_else(|| self.notdef_cid(code, length))
            .unwrap_or(0)
    }

    /// The CID for a code whose byte length is no longer known. Within one
    /// CMap a value can only have come from one length — the codespace
    /// splits greedily, so a two-byte code never starts with a byte that is
    /// a complete one-byte code — so the shortest length whose codespace
    /// holds it is the one.
    pub fn cid_of(&self, code: u32) -> u32 {
        for length in 1..=4usize {
            if length < 4 && code >> (8 * length) != 0 {
                continue;
            }
            if self.in_codespace(length - 1, code) {
                return self.cid(code, length);
            }
        }
        0
    }

    fn mapped(&self, code: u32, length: usize) -> Option<u32> {
        find(self.ranges.get(length.wrapping_sub(1))?, code)
            .map(|(lo, cid)| cid + (code - lo))
            .or_else(|| self.parent.as_ref()?.mapped(code, length))
    }

    fn notdef_cid(&self, code: u32, length: usize) -> Option<u32> {
        find(self.notdef.get(length.wrapping_sub(1))?, code)
            .map(|(_, cid)| cid)
            .or_else(|| self.parent.as_ref()?.notdef_cid(code, length))
    }
}

/// The range containing `code` in a sorted, non-overlapping list, as
/// `(first code, CID)`.
fn find(ranges: &[(u32, u32, u32)], code: u32) -> Option<(u32, u32)> {
    let index = ranges.partition_point(|&(lo, _, _)| lo <= code);
    let &(lo, hi, cid) = ranges[..index].last()?;
    (code <= hi).then_some((lo, cid))
}

/// Flatten ranges given in file order into a sorted, non-overlapping list in
/// which a later range overrides any earlier one it overlaps — the reading
/// order PDF writers rely on when they patch a big range with single codes.
fn normalize(ranges: Vec<(u32, u32, u32)>) -> Vec<(u32, u32, u32)> {
    let mut map: std::collections::BTreeMap<u32, (u32, u32)> = Default::default();
    for (lo, hi, cid) in ranges {
        // Ranges overlapping [lo, hi]: those starting inside it, plus the one
        // starting before it that may reach into it.
        let mut overlapping: Vec<u32> = map.range(lo..=hi).map(|(&start, _)| start).collect();
        if let Some((&start, &(end, _))) = map.range(..lo).next_back() {
            if end >= lo {
                overlapping.push(start);
            }
        }
        for start in overlapping {
            let (end, first_cid) = map.remove(&start).unwrap_or_default();
            if start < lo {
                map.insert(start, (lo - 1, first_cid));
            }
            if end > hi {
                map.insert(hi + 1, (end, first_cid + (hi + 1 - start)));
            }
        }
        map.insert(lo, (hi, cid));
    }
    map.into_iter()
        .map(|(lo, (hi, cid))| (lo, hi, cid))
        .collect()
}

/// What a CMap file contains, before it is turned into a [`CMap`] or a
/// Unicode table.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParsedCMap {
    pub name: String,
    pub vertical: bool,
    pub parent: Option<String>,
    /// `(length, first code, last code)`.
    pub codespace: Vec<(u8, u32, u32)>,
    /// `(length, first code, last code, first CID)`, in file order.
    pub cid_ranges: Vec<(u8, u32, u32, u32)>,
    /// `(length, first code, last code, CID)` from `notdefrange`.
    pub notdef_ranges: Vec<(u8, u32, u32, u32)>,
    /// `(first code, last code, UTF-16 of the first)`: each later code in the
    /// range adds one to the final UTF-16 unit.
    pub unicode_ranges: Vec<(u32, u32, Vec<u16>)>,
}

/// Parse a CMap or ToUnicode CMap program.
pub fn parse(data: &[u8]) -> ParsedCMap {
    let mut out = ParsedCMap::default();
    let mut lexer = Lexer::new(data);
    let mut section: Option<&'static str> = None;
    let mut operands: Vec<Token> = Vec::new();
    // `/Name` tokens just before `usecmap` or a `def`, for the few scalar
    // entries that matter.
    let mut previous: Vec<Token> = Vec::new();
    // An array-form bfrange destination being collected.
    let mut array: Option<Vec<Vec<u8>>> = None;

    while let Ok(Some(spanned)) = lexer.next_token() {
        let token = spanned.token;
        if let Some(items) = array.as_mut() {
            match token {
                Token::HexString(bytes) | Token::LiteralString(bytes) => items.push(bytes),
                Token::ArrayEnd => {
                    let items = array.take().unwrap_or_default();
                    if let (Some(lo), Some(hi)) = (operands.first(), operands.get(1)) {
                        let (lo, hi) = (hex_value(lo), hex_value(hi));
                        if let (Some(lo), Some(hi)) = (lo, hi) {
                            for (offset, units) in items.iter().enumerate() {
                                let code = lo + offset as u32;
                                if code > hi {
                                    break;
                                }
                                out.unicode_ranges.push((code, code, utf16_units(units)));
                            }
                        }
                    }
                    operands.clear();
                }
                _ => {}
            }
            continue;
        }
        match token {
            Token::Keyword(ref keyword) => {
                match keyword.as_str() {
                    "begincodespacerange" => section = Some("codespace"),
                    "begincidrange" => section = Some("cidrange"),
                    "begincidchar" => section = Some("cidchar"),
                    "beginbfrange" => section = Some("bfrange"),
                    "beginbfchar" => section = Some("bfchar"),
                    "beginnotdefrange" => section = Some("notdefrange"),
                    "endcodespacerange" | "endcidrange" | "endcidchar" | "endbfrange"
                    | "endbfchar" | "endnotdefrange" => section = None,
                    "usecmap" => {
                        if let Some(Token::Name(name)) = previous.last() {
                            out.parent = Some(name.clone());
                        }
                    }
                    "def" => {
                        let n = previous.len();
                        if n >= 2 {
                            match (&previous[n - 2], &previous[n - 1]) {
                                (Token::Name(key), Token::Name(value)) if key == "CMapName" => {
                                    out.name = value.clone();
                                }
                                (Token::Name(key), Token::Integer(mode)) if key == "WMode" => {
                                    out.vertical = *mode == 1;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                operands.clear();
                previous.clear();
                continue;
            }
            Token::ArrayStart if section == Some("bfrange") && operands.len() == 2 => {
                array = Some(Vec::new());
                continue;
            }
            _ => {}
        }
        previous.push(token.clone());
        if previous.len() > 4 {
            previous.remove(0);
        }
        let Some(kind) = section else {
            continue;
        };
        operands.push(token);
        match kind {
            "codespace" if operands.len() == 2 => {
                if let (Token::HexString(lo), Token::HexString(hi)) = (&operands[0], &operands[1]) {
                    if (1..=4).contains(&lo.len()) && lo.len() == hi.len() {
                        out.codespace.push((lo.len() as u8, be(lo), be(hi)));
                    }
                }
                operands.clear();
            }
            "cidrange" | "notdefrange" if operands.len() == 3 => {
                if let (Token::HexString(lo), Token::HexString(hi), Token::Integer(cid)) =
                    (&operands[0], &operands[1], &operands[2])
                {
                    if (1..=4).contains(&lo.len()) && *cid >= 0 && be(hi) >= be(lo) {
                        // A cidrange counts up from its CID; a notdefrange sends
                        // the whole range to that one CID.
                        let range = (lo.len() as u8, be(lo), be(hi), *cid as u32);
                        if kind == "cidrange" {
                            out.cid_ranges.push(range);
                        } else {
                            out.notdef_ranges.push(range);
                        }
                    }
                }
                operands.clear();
            }
            "cidchar" if operands.len() == 2 => {
                if let (Token::HexString(code), Token::Integer(cid)) = (&operands[0], &operands[1])
                {
                    if (1..=4).contains(&code.len()) && *cid >= 0 {
                        let value = be(code);
                        out.cid_ranges
                            .push((code.len() as u8, value, value, *cid as u32));
                    }
                }
                operands.clear();
            }
            "bfchar" if operands.len() == 2 => {
                if let (Some(code), Token::HexString(dst) | Token::LiteralString(dst)) =
                    (hex_value(&operands[0]), &operands[1])
                {
                    out.unicode_ranges.push((code, code, utf16_units(dst)));
                } else if let (Some(code), Token::Name(name)) =
                    (hex_value(&operands[0]), &operands[1])
                {
                    // Some writers give a glyph name instead of a string.
                    if let Some(ch) = crate::font::char_for_glyph_name(name) {
                        let mut units = [0u16; 2];
                        out.unicode_ranges
                            .push((code, code, ch.encode_utf16(&mut units).to_vec()));
                    }
                }
                operands.clear();
            }
            "bfrange" if operands.len() == 3 => {
                if let (Some(lo), Some(hi), Token::HexString(dst)) = (
                    hex_value(&operands[0]),
                    hex_value(&operands[1]),
                    &operands[2],
                ) {
                    if hi >= lo && hi - lo <= 0xFFFF {
                        out.unicode_ranges.push((lo, hi, utf16_units(dst)));
                    }
                }
                operands.clear();
            }
            _ => {
                if operands.len() > 3 {
                    operands.clear();
                }
            }
        }
    }
    out
}

impl ParsedCMap {
    /// Build the code → CID map, resolving `usecmap` through `parent`.
    pub fn into_cmap(self, parent: Option<Arc<CMap>>) -> CMap {
        let mut cmap = CMap {
            name: self.name,
            vertical: self.vertical,
            parent,
            ..CMap::default()
        };
        for (length, lo, hi) in self.codespace {
            cmap.codespace[usize::from(length) - 1].push((lo, hi));
        }
        let mut ranges: [Vec<(u32, u32, u32)>; 4] = Default::default();
        for (length, lo, hi, cid) in self.cid_ranges {
            ranges[usize::from(length) - 1].push((lo, hi, cid));
        }
        let mut notdef: [Vec<(u32, u32, u32)>; 4] = Default::default();
        for (length, lo, hi, cid) in self.notdef_ranges {
            notdef[usize::from(length) - 1].push((lo, hi, cid));
        }
        // A damaged embedded CMap may leave out its codespace. Without one
        // every byte would be its own code; the extent of each length's
        // mappings is a far better guess at what the writer meant.
        if !cmap.has_codespace() {
            for (index, list) in ranges.iter().enumerate() {
                let lo = list.iter().map(|r| r.0).min();
                let hi = list.iter().map(|r| r.1).max();
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    cmap.codespace[index].push((lo, hi));
                }
            }
        }
        cmap.ranges = ranges.map(normalize);
        cmap.notdef = notdef.map(normalize);
        cmap
    }

    /// Expand the bfchar/bfrange entries into a code → text table.
    pub fn unicode_table(&self) -> HashMap<u32, String> {
        let mut table = HashMap::new();
        for (lo, hi, first) in &self.unicode_ranges {
            let mut units = first.clone();
            for code in *lo..=*hi {
                table.insert(code, String::from_utf16_lossy(&units));
                if let Some(last) = units.last_mut() {
                    *last = last.wrapping_add(1);
                }
            }
        }
        table
    }
}

fn hex_value(token: &Token) -> Option<u32> {
    match token {
        Token::HexString(bytes) if bytes.len() <= 4 => Some(be(bytes)),
        _ => None,
    }
}

fn be(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b))
}

/// UTF-16BE units from a bfchar/bfrange destination. An odd trailing byte is
/// a writer's mistake; keeping it as a unit beats dropping the character.
fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|c| match c {
            [hi, lo] => u16::from_be_bytes([*hi, *lo]),
            [b] => u16::from(*b),
            _ => 0,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Predefined CMaps
// ---------------------------------------------------------------------------

/// Adobe's predefined CMaps and `*-UCS2` tables, compiled by the
/// `build_cmap_bundle` example from adobe-type-tools/cmap-resources and
/// mapping-resources-pdf (BSD-3-Clause; see `data/LICENSE-adobe-cmaps.txt`).
static BUNDLE: &[u8] = include_bytes!("../data/cmaps.bin");

const MAGIC: &[u8; 8] = b"PDFCMAP1";

/// A predefined CMap by name — `UniGB-UCS2-H`, `90ms-RKSJ-H`, `Identity-V` —
/// parsed once per process and shared.
pub fn predefined(name: &str) -> Option<Arc<CMap>> {
    match name {
        "Identity-H" => return Some(identity_shared(false)),
        "Identity-V" => return Some(identity_shared(true)),
        _ => {}
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<CMap>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().ok()?.get(name) {
        return found.clone();
    }
    let loaded = load_predefined(name, 0);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(name.to_owned(), loaded.clone());
    }
    loaded
}

fn identity_shared(vertical: bool) -> Arc<CMap> {
    static H: OnceLock<Arc<CMap>> = OnceLock::new();
    static V: OnceLock<Arc<CMap>> = OnceLock::new();
    let cell = if vertical { &V } else { &H };
    Arc::clone(cell.get_or_init(|| Arc::new(CMap::identity(vertical))))
}

fn load_predefined(name: &str, depth: usize) -> Option<Arc<CMap>> {
    if depth > 8 {
        return None;
    }
    let parsed = bundle_entry(name)?;
    let parent = match parsed.parent.as_deref() {
        Some(parent) => Some(predefined_at_depth(parent, depth + 1)?),
        None => None,
    };
    Some(Arc::new(parsed.into_cmap(parent)))
}

fn predefined_at_depth(name: &str, depth: usize) -> Option<Arc<CMap>> {
    if depth == 0 {
        predefined(name)
    } else {
        match name {
            "Identity-H" => Some(identity_shared(false)),
            "Identity-V" => Some(identity_shared(true)),
            _ => load_predefined(name, depth),
        }
    }
}

/// The CID → Unicode table for a character collection, e.g. `Adobe-Japan1`,
/// used to extract text from CJK fonts that carry no ToUnicode.
pub fn cid_to_unicode(registry_ordering: &str) -> Option<Arc<HashMap<u32, String>>> {
    let table_name = match registry_ordering {
        "Adobe-GB1" => "Adobe-GB1-UCS2",
        "Adobe-CNS1" => "Adobe-CNS1-UCS2",
        "Adobe-Japan1" => "Adobe-Japan1-UCS2",
        "Adobe-Korea1" => "Adobe-Korea1-UCS2",
        "Adobe-KR" => "Adobe-KR-UCS2",
        _ => return None,
    };
    type Tables = HashMap<&'static str, Arc<HashMap<u32, String>>>;
    static CACHE: OnceLock<Mutex<Tables>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().ok()?.get(table_name) {
        return Some(Arc::clone(found));
    }
    let table = Arc::new(bundle_entry(table_name)?.unicode_table());
    if let Ok(mut cache) = cache.lock() {
        cache.insert(table_name, Arc::clone(&table));
    }
    Some(table)
}

/// For a predefined CMap whose codes *are* Unicode (`UniGB-UCS2-H`,
/// `UniJIS-UTF16-V`, …), how they are spelled — so text can be read straight
/// off the codes without a ToUnicode CMap or a CID table.
pub fn is_unicode_keyed(name: &str) -> Option<UnicodeForm> {
    let encoding = name
        .strip_suffix("-H")
        .or_else(|| name.strip_suffix("-V"))?;
    if !encoding.starts_with("Uni") {
        return None;
    }
    let form = encoding.rsplit('-').next()?;
    match form {
        "UCS2" | "HW" => Some(UnicodeForm::Ucs2),
        "UTF16" => Some(UnicodeForm::Utf16),
        "UTF8" => Some(UnicodeForm::Utf8),
        "UTF32" => Some(UnicodeForm::Utf32),
        _ => None,
    }
}

/// How a Unicode-keyed predefined CMap spells its codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeForm {
    Ucs2,
    Utf16,
    Utf8,
    Utf32,
}

impl UnicodeForm {
    /// The text a code of this form spells.
    pub fn decode(self, code: u32, length: usize) -> Option<String> {
        match self {
            UnicodeForm::Ucs2 | UnicodeForm::Utf32 => char::from_u32(code).map(String::from),
            UnicodeForm::Utf16 => {
                if length == 4 {
                    let units = [(code >> 16) as u16, code as u16];
                    String::from_utf16(&units).ok()
                } else {
                    char::from_u32(code).map(String::from)
                }
            }
            UnicodeForm::Utf8 => {
                let bytes = code.to_be_bytes();
                std::str::from_utf8(&bytes[4 - length.min(4)..])
                    .ok()
                    .map(str::to_owned)
            }
        }
    }
}

/// The bundle is an index — magic, then per entry its name, offset and
/// length — followed by entries each deflated on their own, so opening one
/// CMap never inflates the other two hundred.
fn bundle_index() -> &'static HashMap<String, (usize, usize)> {
    static INDEX: OnceLock<HashMap<String, (usize, usize)>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        let Some(rest) = BUNDLE.strip_prefix(MAGIC.as_slice()) else {
            return index;
        };
        let mut reader = Reader::new(rest);
        let Some(count) = reader.varint() else {
            return index;
        };
        let mut entries = Vec::new();
        for _ in 0..count {
            let (Some(name), Some(length)) = (reader.string(), reader.varint()) else {
                return HashMap::new();
            };
            entries.push((name, length as usize));
        }
        let mut offset = MAGIC.len() + reader.position;
        for (name, length) in entries {
            index.insert(name, (offset, length));
            offset += length;
        }
        index
    })
}

fn bundle_entry(name: &str) -> Option<ParsedCMap> {
    let &(offset, length) = bundle_index().get(name)?;
    let compressed = BUNDLE.get(offset..offset + length)?;
    let bytes = pdf_core::filter::flate_decode(compressed).ok()?;
    decode_entry(&bytes)
}

/// Serialise parsed CMaps into the bundle format [`predefined`] reads. Used
/// by the generator; kept beside the reader so the two cannot drift apart.
pub fn encode_bundle(entries: &[ParsedCMap]) -> Vec<u8> {
    let bodies: Vec<Vec<u8>> = entries
        .iter()
        .map(|entry| pdf_core::filter::flate_encode(&encode_entry(entry)))
        .collect();
    let mut out = MAGIC.to_vec();
    put_varint(&mut out, entries.len() as u64);
    for (entry, body) in entries.iter().zip(&bodies) {
        put_string(&mut out, &entry.name);
        put_varint(&mut out, body.len() as u64);
    }
    for body in bodies {
        out.extend_from_slice(&body);
    }
    out
}

fn encode_entry(entry: &ParsedCMap) -> Vec<u8> {
    let mut out = Vec::new();
    put_string(&mut out, &entry.name);
    put_string(&mut out, entry.parent.as_deref().unwrap_or(""));
    out.push(u8::from(entry.vertical));
    put_varint(&mut out, entry.codespace.len() as u64);
    for &(length, lo, hi) in &entry.codespace {
        out.push(length);
        put_varint(&mut out, u64::from(lo));
        put_varint(&mut out, u64::from(hi));
    }
    // CID ranges, delta-coded: most continue where the previous one stopped,
    // so the start and first CID usually encode as zero.
    put_varint(&mut out, entry.cid_ranges.len() as u64);
    let (mut next_code, mut next_cid) = (0i64, 0i64);
    for &(length, lo, hi, cid) in &entry.cid_ranges {
        out.push(length);
        put_signed(&mut out, i64::from(lo) - next_code);
        put_varint(&mut out, u64::from(hi - lo));
        put_signed(&mut out, i64::from(cid) - next_cid);
        next_code = i64::from(hi) + 1;
        next_cid = i64::from(cid) + i64::from(hi - lo) + 1;
    }
    put_varint(&mut out, entry.notdef_ranges.len() as u64);
    for &(length, lo, hi, cid) in &entry.notdef_ranges {
        out.push(length);
        put_varint(&mut out, u64::from(lo));
        put_varint(&mut out, u64::from(hi - lo));
        put_varint(&mut out, u64::from(cid));
    }
    put_varint(&mut out, entry.unicode_ranges.len() as u64);
    let mut next_code = 0i64;
    for (lo, hi, units) in &entry.unicode_ranges {
        put_signed(&mut out, i64::from(*lo) - next_code);
        put_varint(&mut out, u64::from(hi - lo));
        put_varint(&mut out, units.len() as u64);
        for &unit in units {
            put_varint(&mut out, u64::from(unit));
        }
        next_code = i64::from(*hi) + 1;
    }
    out
}

fn decode_entry(bytes: &[u8]) -> Option<ParsedCMap> {
    let mut r = Reader::new(bytes);
    let name = r.string()?;
    let parent = Some(r.string()?).filter(|p| !p.is_empty());
    let vertical = r.byte()? == 1;
    let mut codespace = Vec::new();
    for _ in 0..r.varint()? {
        let length = r.byte()?;
        codespace.push((length, r.varint()? as u32, r.varint()? as u32));
    }
    let mut cid_ranges = Vec::new();
    let (mut next_code, mut next_cid) = (0i64, 0i64);
    for _ in 0..r.varint()? {
        let length = r.byte()?;
        if !(1..=4).contains(&length) {
            return None;
        }
        let lo = next_code + r.signed()?;
        let span = r.varint()? as i64;
        let cid = next_cid + r.signed()?;
        let (lo32, cid32) = (u32::try_from(lo).ok()?, u32::try_from(cid).ok()?);
        cid_ranges.push((length, lo32, lo32.checked_add(span as u32)?, cid32));
        next_code = lo + span + 1;
        next_cid = cid + span + 1;
    }
    let mut notdef_ranges = Vec::new();
    for _ in 0..r.varint()? {
        let length = r.byte()?;
        if !(1..=4).contains(&length) {
            return None;
        }
        let lo = u32::try_from(r.varint()?).ok()?;
        let hi = lo.checked_add(u32::try_from(r.varint()?).ok()?)?;
        notdef_ranges.push((length, lo, hi, u32::try_from(r.varint()?).ok()?));
    }
    let mut unicode_ranges = Vec::new();
    let mut next_code = 0i64;
    for _ in 0..r.varint()? {
        let lo = next_code + r.signed()?;
        let span = r.varint()? as i64;
        let count = r.varint()? as usize;
        let mut units = Vec::with_capacity(count.min(16));
        for _ in 0..count {
            units.push(r.varint()? as u16);
        }
        let lo32 = u32::try_from(lo).ok()?;
        unicode_ranges.push((lo32, lo32.checked_add(span as u32)?, units));
        next_code = lo + span + 1;
    }
    Some(ParsedCMap {
        name,
        vertical,
        parent,
        codespace,
        cid_ranges,
        notdef_ranges,
        unicode_ranges,
    })
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_signed(out: &mut Vec<u8>, value: i64) {
    put_varint(out, ((value << 1) ^ (value >> 63)) as u64);
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, position: 0 }
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn signed(&mut self) -> Option<i64> {
        let raw = self.varint()?;
        Some((raw >> 1) as i64 ^ -((raw & 1) as i64))
    }

    fn string(&mut self) -> Option<String> {
        let length = self.varint()? as usize;
        let bytes = self.bytes.get(self.position..self.position + length)?;
        self.position += length;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT_JIS_LIKE: &[u8] = b"
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CMapName /Test-RKSJ-H def
/WMode 0 def
2 begincodespacerange
<00> <80>
<8140> <9FFC>
endcodespacerange
2 begincidrange
<20> <7e> 231
<8140> <817e> 633
endcidrange
1 begincidchar
<8180> 696
endcidchar
endcmap
";

    #[test]
    fn mixed_width_codes_split_by_codespace() {
        let cmap = parse(SHIFT_JIS_LIKE).into_cmap(None);
        assert_eq!(cmap.name, "Test-RKSJ-H");
        // "A" is one byte; 0x8142 is a two-byte code.
        let bytes = [0x41, 0x81, 0x42, 0x41];
        let (code, n) = cmap.next_code(&bytes);
        assert_eq!((code, n), (0x41, 1));
        let (code, n) = cmap.next_code(&bytes[1..]);
        assert_eq!((code, n), (0x8142, 2));
        assert_eq!(cmap.cid(0x41, 1), 231 + (0x41 - 0x20));
        assert_eq!(cmap.cid(0x8142, 2), 635);
        assert_eq!(cmap.cid(0x8180, 2), 696);
        assert_eq!(cmap.cid(0x9000, 2), 0, "unmapped codes select .notdef");
    }

    /// A big range patched with many single codes: every patch wins, and the
    /// rest of the big range is still found however many patches follow it.
    #[test]
    fn later_entries_override_earlier_overlapping_ranges() {
        let mut source = String::from(
            "1 begincodespacerange <0000> <FFFF> endcodespacerange
1 begincidrange <1000> <1fff> 100 endcidrange
20 begincidchar\n",
        );
        for i in 0..20u32 {
            source.push_str(&format!("<{:04x}> {}\n", 0x1010 + i * 2, 50_000 + i));
        }
        source.push_str("endcidchar");
        let cmap = parse(source.as_bytes()).into_cmap(None);
        assert_eq!(cmap.cid(0x1010, 2), 50_000, "patched");
        assert_eq!(cmap.cid(0x1011, 2), 100 + 0x11, "between patches");
        assert_eq!(cmap.cid(0x1fff, 2), 100 + 0xfff, "past every patch");
        assert_eq!(cmap.cid(0x1000, 2), 100, "before every patch");
    }

    #[test]
    fn normalize_splits_an_overlapped_range() {
        let ranges = normalize(vec![(0, 9, 100), (3, 4, 7)]);
        assert_eq!(ranges, vec![(0, 2, 100), (3, 4, 7), (5, 9, 105)]);
    }

    #[test]
    fn notdef_ranges_only_catch_unmapped_codes() {
        let cmap = parse(
            b"1 begincodespacerange <00> <ff> endcodespacerange
1 begincidrange <20> <7e> 1 endcidrange
1 beginnotdefrange <00> <ff> 999 endnotdefrange",
        )
        .into_cmap(None);
        assert_eq!(cmap.cid(0x41, 1), 1 + 0x21, "a real mapping beats notdef");
        assert_eq!(
            cmap.cid(0x05, 1),
            999,
            "an unmapped code gets the notdef CID"
        );
    }

    #[test]
    fn a_missing_codespace_is_inferred_from_the_mappings() {
        let cmap = parse(b"1 begincidrange <8140> <817e> 633 endcidrange").into_cmap(None);
        assert_eq!(cmap.next_code(&[0x81, 0x42]), (0x8142, 2));
        assert_eq!(cmap.cid_of(0x8142), 635);
    }

    #[test]
    fn cid_of_finds_the_length_from_the_codespace() {
        let cmap = parse(SHIFT_JIS_LIKE).into_cmap(None);
        assert_eq!(cmap.cid_of(0x41), 231 + 0x21);
        assert_eq!(cmap.cid_of(0x8142), 635);
    }

    #[test]
    fn bytes_outside_every_codespace_advance_one_at_a_time() {
        let cmap = parse(SHIFT_JIS_LIKE).into_cmap(None);
        assert_eq!(cmap.next_code(&[0xFF, 0x41]), (0xFF, 1));
    }

    #[test]
    fn usecmap_falls_back_to_the_parent() {
        let parent = Arc::new(parse(SHIFT_JIS_LIKE).into_cmap(None));
        let child = parse(
            b"/Test-RKSJ-H usecmap
1 begincidchar
<8141> 9999
endcidchar",
        );
        assert_eq!(child.parent.as_deref(), Some("Test-RKSJ-H"));
        let child = child.into_cmap(Some(parent));
        assert_eq!(child.cid(0x8141, 2), 9999, "the child's own entry wins");
        assert_eq!(child.cid(0x8142, 2), 635, "the rest comes from the parent");
        assert_eq!(child.next_code(&[0x81, 0x42]), (0x8142, 2));
    }

    #[test]
    fn identity_is_two_bytes_per_code_and_code_equals_cid() {
        let cmap = CMap::identity(false);
        assert_eq!(cmap.next_code(&[0x12, 0x34]), (0x1234, 2));
        assert_eq!(cmap.cid(0x1234, 2), 0x1234);
    }

    #[test]
    fn bfrange_array_form_is_not_dropped() {
        let parsed = parse(
            b"1 beginbfrange
<0010> <0012> [<0041> <00660069> <0043>]
endbfrange",
        );
        let table = parsed.unicode_table();
        assert_eq!(table.get(&0x10).map(String::as_str), Some("A"));
        assert_eq!(table.get(&0x11).map(String::as_str), Some("fi"));
        assert_eq!(table.get(&0x12).map(String::as_str), Some("C"));
    }

    #[test]
    fn bfrange_increments_the_last_unit() {
        let table = parse(b"1 beginbfrange <20> <22> <0061> endbfrange").unicode_table();
        assert_eq!(table.get(&0x22).map(String::as_str), Some("c"));
    }

    #[test]
    fn wmode_one_is_vertical() {
        let cmap = parse(b"/WMode 1 def").into_cmap(None);
        assert!(cmap.vertical);
    }

    #[test]
    fn bundle_round_trips() {
        let entry = parse(SHIFT_JIS_LIKE);
        let bundle = encode_bundle(std::slice::from_ref(&entry));
        let rest = bundle.strip_prefix(MAGIC.as_slice()).unwrap();
        let mut reader = Reader::new(rest);
        assert_eq!(reader.varint(), Some(1));
        assert_eq!(reader.string().as_deref(), Some("Test-RKSJ-H"));
        let length = reader.varint().unwrap() as usize;
        let start = MAGIC.len() + reader.position;
        let body = pdf_core::filter::flate_decode(&bundle[start..start + length]).unwrap();
        assert_eq!(decode_entry(&body), Some(entry));
    }

    #[test]
    fn unicode_keyed_cmaps_decode_their_codes() {
        assert_eq!(is_unicode_keyed("UniGB-UCS2-H"), Some(UnicodeForm::Ucs2));
        assert_eq!(is_unicode_keyed("UniJIS-UTF16-V"), Some(UnicodeForm::Utf16));
        assert_eq!(is_unicode_keyed("90ms-RKSJ-H"), None);
        // U+1F600 as a UTF-16 surrogate pair in one four-byte code.
        let text = UnicodeForm::Utf16.decode(0xD83D_DE00, 4);
        assert_eq!(text.as_deref(), Some("\u{1F600}"));
        assert_eq!(UnicodeForm::Ucs2.decode(0x4E2D, 2).as_deref(), Some("中"));
    }
}
