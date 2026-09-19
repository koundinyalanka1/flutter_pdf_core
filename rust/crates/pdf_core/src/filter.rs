//! Milestone 7: stream filters.
//!
//! Supports FlateDecode and LZWDecode (both with PNG/TIFF predictors),
//! RunLengthDecode, ASCIIHexDecode and ASCII85Decode. Filter chains
//! (`/Filter` as an array) are applied in order.

use std::io::Read;
use std::io::Write as _;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, PdfObject};

/// Decode parameters relevant to FlateDecode predictors.
#[derive(Debug, Clone, Copy)]
pub struct DecodeParms {
    pub predictor: u8,
    pub colors: usize,
    pub bits_per_component: usize,
    pub columns: usize,
    /// LZW only: whether code width grows one code earlier than the table
    /// strictly requires. `/EarlyChange 0` turns it off; the default is on,
    /// and getting it wrong shifts every code after the first table growth.
    pub early_change: bool,
}

impl Default for DecodeParms {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
            early_change: true,
        }
    }
}

impl DecodeParms {
    pub fn from_dict(dict: &Dictionary) -> Self {
        let get = |key: &str, default: i64| -> i64 {
            dict.get(key).and_then(PdfObject::as_i64).unwrap_or(default)
        };
        Self {
            predictor: get("Predictor", 1).clamp(1, 15) as u8,
            colors: get("Colors", 1).max(1) as usize,
            bits_per_component: get("BitsPerComponent", 8).max(1) as usize,
            columns: get("Columns", 1).max(1) as usize,
            early_change: get("EarlyChange", 1) != 0,
        }
    }
}

/// Apply a single named filter.
pub fn decode(filter: &str, data: &[u8], parms: &DecodeParms) -> Result<Vec<u8>> {
    match filter {
        "FlateDecode" | "Fl" => {
            let inflated = flate_decode(data)?;
            apply_predictor(&inflated, parms)
        }
        "LZWDecode" | "LZW" => {
            let expanded = lzw_decode(data, parms.early_change)?;
            apply_predictor(&expanded, parms)
        }
        "RunLengthDecode" | "RL" => run_length_decode(data),
        "ASCIIHexDecode" | "AHx" => ascii_hex_decode(data),
        "ASCII85Decode" | "A85" => ascii85_decode(data),
        // Decryption already ran when the document was opened, so a /Crypt
        // filter has nothing left to do here.
        "Crypt" => Ok(data.to_vec()),
        other => Err(PdfError::UnsupportedFilter(other.to_owned())),
    }
}

/// Decode stream data given its (already direct) dictionary.
pub fn decode_with_dict(dict: &Dictionary, data: &[u8]) -> Result<Vec<u8>> {
    let filters = filter_names(dict);
    if filters.is_empty() {
        return Ok(data.to_vec());
    }
    let parms_list = decode_parms_list(dict, filters.len());
    let mut current = data.to_vec();
    for (i, name) in filters.iter().enumerate() {
        let parms = parms_list.get(i).copied().unwrap_or_default();
        current = decode(name, &current, &parms)?;
    }
    Ok(current)
}

fn filter_names(dict: &Dictionary) -> Vec<String> {
    match dict.get("Filter") {
        Some(PdfObject::Name(name)) => vec![name.clone()],
        Some(PdfObject::Array(items)) => items
            .iter()
            .filter_map(|o| o.as_name().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_parms_list(dict: &Dictionary, n: usize) -> Vec<DecodeParms> {
    let mut out = vec![DecodeParms::default(); n];
    match dict.get("DecodeParms").or_else(|| dict.get("DP")) {
        Some(PdfObject::Dictionary(d)) => {
            if n > 0 {
                out[0] = DecodeParms::from_dict(d);
            }
        }
        Some(PdfObject::Array(items)) => {
            for (i, item) in items.iter().enumerate().take(n) {
                if let PdfObject::Dictionary(d) = item {
                    out[i] = DecodeParms::from_dict(d);
                }
            }
        }
        _ => {}
    }
    out
}

pub fn flate_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut decoder = ZlibDecoder::new(data);
    match decoder.read_to_end(&mut out) {
        Ok(_) => Ok(out),
        // A truncated stream still inflated everything up to the damage, and
        // most of a page is worth more than none of it. This matches how
        // LZWDecode and the CCITT decoder treat short input.
        Err(_) if !out.is_empty() => Ok(out),
        Err(e) => Err(PdfError::Filter(format!("FlateDecode failed: {e}"))),
    }
}

pub fn flate_encode(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .expect("writing to in-memory encoder cannot fail");
    encoder
        .finish()
        .expect("finishing in-memory encoder cannot fail")
}

// ---------------------------------------------------------------------------
// LZWDecode
// ---------------------------------------------------------------------------

const LZW_CLEAR: u16 = 256;
const LZW_EOD: u16 = 257;
const LZW_MAX: usize = 4096;

/// LZW as PDF uses it: MSB-first codes, 9 bits growing to 12, 256 = clear
/// table, 257 = end of data.
///
/// Damaged or truncated streams stop at the damage and return what was
/// recovered rather than erroring, because a partly-decoded content stream
/// still draws most of a page.
pub fn lzw_decode(data: &[u8], early_change: bool) -> Result<Vec<u8>> {
    let early = usize::from(early_change);
    let mut table: Vec<Vec<u8>> = Vec::with_capacity(LZW_MAX);
    reset_lzw_table(&mut table);

    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 3);
    let mut width = 9usize;
    let mut previous: Option<u16> = None;
    let mut bit = 0usize;
    let total_bits = data.len() * 8;

    while bit + width <= total_bits {
        let mut code = 0u16;
        for i in 0..width {
            let at = bit + i;
            let value = (data[at >> 3] >> (7 - (at & 7))) & 1;
            code = (code << 1) | u16::from(value);
        }
        bit += width;

        match code {
            LZW_CLEAR => {
                reset_lzw_table(&mut table);
                width = 9;
                previous = None;
                continue;
            }
            LZW_EOD => break,
            _ => {}
        }

        // Either a code already in the table, or the classic "code not yet
        // defined" case, where the entry is the previous one plus its own
        // first byte.
        let entry: Vec<u8> = match table.get(code as usize) {
            Some(existing) if !existing.is_empty() => existing.clone(),
            _ => {
                let Some(prev) = previous else { break };
                let Some(base) = table.get(prev as usize) else { break };
                if base.is_empty() {
                    break;
                }
                let mut built = base.clone();
                built.push(base[0]);
                built
            }
        };
        out.extend_from_slice(&entry);

        if let Some(prev) = previous {
            if table.len() < LZW_MAX {
                if let Some(base) = table.get(prev as usize) {
                    let mut grown = base.clone();
                    grown.push(entry[0]);
                    table.push(grown);
                }
            }
        }
        previous = Some(code);

        width = match table.len() + early {
            n if n >= 2048 => 12,
            n if n >= 1024 => 11,
            n if n >= 512 => 10,
            _ => 9,
        };
    }

    Ok(out)
}

fn reset_lzw_table(table: &mut Vec<Vec<u8>>) {
    table.clear();
    for byte in 0..=255u16 {
        table.push(vec![byte as u8]);
    }
    // 256 and 257 are the clear and EOD markers; they hold no data but must
    // occupy their slots so later codes land at the right index.
    table.push(Vec::new());
    table.push(Vec::new());
}

// ---------------------------------------------------------------------------
// RunLengthDecode
// ---------------------------------------------------------------------------

/// Length byte `n`: 0–127 means copy the next `n + 1` bytes literally,
/// 129–255 means repeat the next byte `257 - n` times, and 128 ends the data.
pub fn run_length_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0usize;
    while i < data.len() {
        let length = data[i];
        i += 1;
        match length {
            128 => break,
            0..=127 => {
                let take = usize::from(length) + 1;
                let end = (i + take).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
            }
            _ => {
                let Some(&byte) = data.get(i) else { break };
                out.extend(std::iter::repeat(byte).take(257 - usize::from(length)));
                i += 1;
            }
        }
    }
    Ok(out)
}

fn apply_predictor(data: &[u8], parms: &DecodeParms) -> Result<Vec<u8>> {
    match parms.predictor {
        1 => Ok(data.to_vec()),
        2 => tiff_predictor(data, parms),
        10..=15 => png_predictor(data, parms),
        other => Err(PdfError::Filter(format!("unsupported predictor {other}"))),
    }
}

fn bytes_per_pixel(parms: &DecodeParms) -> usize {
    (parms.colors * parms.bits_per_component).div_ceil(8)
}

fn row_len(parms: &DecodeParms) -> usize {
    (parms.columns * parms.colors * parms.bits_per_component).div_ceil(8)
}

fn tiff_predictor(data: &[u8], parms: &DecodeParms) -> Result<Vec<u8>> {
    if parms.bits_per_component != 8 {
        return Err(PdfError::Filter(
            "TIFF predictor only supported for 8 bits per component".into(),
        ));
    }
    let row = row_len(parms);
    let bpp = bytes_per_pixel(parms);
    let mut out = data.to_vec();
    for r in out.chunks_mut(row) {
        for i in bpp..r.len() {
            r[i] = r[i].wrapping_add(r[i - bpp]);
        }
    }
    Ok(out)
}

fn png_predictor(data: &[u8], parms: &DecodeParms) -> Result<Vec<u8>> {
    let row = row_len(parms);
    let bpp = bytes_per_pixel(parms).max(1);
    if row == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut prev_row = vec![0u8; row];
    let mut pos = 0;
    while pos < data.len() {
        let ft = data[pos];
        pos += 1;
        let end = (pos + row).min(data.len());
        let mut current = data[pos..end].to_vec();
        pos = end;
        match ft {
            0 => {}
            1 => {
                for i in bpp..current.len() {
                    current[i] = current[i].wrapping_add(current[i - bpp]);
                }
            }
            2 => {
                for i in 0..current.len() {
                    current[i] = current[i].wrapping_add(prev_row[i]);
                }
            }
            3 => {
                for i in 0..current.len() {
                    let left = if i >= bpp { current[i - bpp] as u16 } else { 0 };
                    let up = prev_row[i] as u16;
                    current[i] = current[i].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                for i in 0..current.len() {
                    let left = if i >= bpp { current[i - bpp] } else { 0 };
                    let up = prev_row[i];
                    let up_left = if i >= bpp { prev_row[i - bpp] } else { 0 };
                    current[i] = current[i].wrapping_add(paeth(left, up, up_left));
                }
            }
            other => {
                return Err(PdfError::Filter(format!("invalid PNG filter type {other}")));
            }
        }
        prev_row.clear();
        prev_row.extend_from_slice(&current);
        prev_row.resize(row, 0);
        out.extend_from_slice(&current);
    }
    Ok(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i16, b as i16, c as i16);
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn ascii_hex_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut nibbles = Vec::new();
    for &b in data {
        match b {
            b'>' => break,
            b if crate::lexer::is_ws(b) => continue,
            b'0'..=b'9' => nibbles.push(b - b'0'),
            b'a'..=b'f' => nibbles.push(b - b'a' + 10),
            b'A'..=b'F' => nibbles.push(b - b'A' + 10),
            other => {
                return Err(PdfError::Filter(format!(
                    "invalid ASCIIHex byte 0x{other:02x}"
                )))
            }
        }
    }
    if nibbles.len() % 2 == 1 {
        nibbles.push(0);
    }
    Ok(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

fn ascii85_decode(data: &[u8]) -> Result<Vec<u8>> {
    // Strip optional <~ prefix.
    let bytes = if data.starts_with(b"<~") {
        &data[2..]
    } else {
        data
    };
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if crate::lexer::is_ws(b) {
            continue;
        }
        if b == b'~' {
            break; // ~> terminator
        }
        if b == b'z' && n == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return Err(PdfError::Filter(format!("invalid ASCII85 byte 0x{b:02x}")));
        }
        group[n] = b - b'!';
        n += 1;
        if n == 5 {
            let value = group.iter().fold(0u32, |acc, &d| acc * 85 + d as u32);
            out.extend_from_slice(&value.to_be_bytes());
            n = 0;
        }
    }
    if n > 0 {
        if n == 1 {
            return Err(PdfError::Filter("truncated ASCII85 group".into()));
        }
        for slot in group.iter_mut().skip(n) {
            *slot = 84;
        }
        let value = group.iter().fold(0u32, |acc, &d| acc * 85 + d as u32);
        out.extend_from_slice(&value.to_be_bytes()[..n - 1]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes with the same 9→12-bit scheme the decoder expects, so the
    /// round trip exercises table growth rather than a hand-picked vector.
    fn lzw_encode(data: &[u8], early_change: bool) -> Vec<u8> {
        let early = usize::from(early_change);
        let mut dict: std::collections::HashMap<Vec<u8>, u16> =
            (0..=255u16).map(|b| (vec![b as u8], b)).collect();
        let mut next = 258u16;
        let mut width = 9usize;
        let mut bits: Vec<u8> = Vec::new();
        let emit = |code: u16, width: usize, bits: &mut Vec<u8>| {
            for i in (0..width).rev() {
                bits.push(((code >> i) & 1) as u8);
            }
        };
        emit(LZW_CLEAR, width, &mut bits);
        let mut current: Vec<u8> = Vec::new();
        for &byte in data {
            let mut candidate = current.clone();
            candidate.push(byte);
            if dict.contains_key(&candidate) {
                current = candidate;
            } else {
                emit(dict[&current], width, &mut bits);
                if (next as usize) < LZW_MAX {
                    dict.insert(candidate, next);
                    next += 1;
                }
                width = match next as usize - 1 + early {
                    n if n >= 2048 => 12,
                    n if n >= 1024 => 11,
                    n if n >= 512 => 10,
                    _ => 9,
                };
                current = vec![byte];
            }
        }
        if !current.is_empty() {
            emit(dict[&current], width, &mut bits);
        }
        emit(LZW_EOD, width, &mut bits);
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b))
            .collect()
    }

    #[test]
    fn lzw_round_trip_grows_the_code_width() {
        // Long enough to push the table past 511 entries and force 10-bit codes.
        let mut payload = Vec::new();
        for i in 0..4000u32 {
            payload.extend_from_slice(format!("token{} ", i % 900).as_bytes());
        }
        let encoded = lzw_encode(&payload, true);
        assert_eq!(lzw_decode(&encoded, true).unwrap(), payload);
    }

    #[test]
    fn lzw_handles_early_change_off() {
        let payload = b"aaabbbcccaaabbbccc-repeat-aaabbbccc".repeat(40);
        let encoded = lzw_encode(&payload, false);
        assert_eq!(lzw_decode(&encoded, false).unwrap(), payload);
    }

    #[test]
    fn lzw_decodes_a_content_stream_through_the_dictionary() {
        let content = b"BT /F1 24 Tf 72 700 Td (LZW) Tj ET".to_vec();
        let mut dict = Dictionary::new();
        dict.insert("Filter".into(), PdfObject::Name("LZWDecode".into()));
        let encoded = lzw_encode(&content, true);
        assert_eq!(decode_with_dict(&dict, &encoded).unwrap(), content);
    }

    #[test]
    fn lzw_truncated_input_returns_what_it_recovered() {
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(20);
        let encoded = lzw_encode(&payload, true);
        let decoded = lzw_decode(&encoded[..encoded.len() / 2], true).unwrap();
        assert!(!decoded.is_empty(), "partial LZW should still yield bytes");
        assert!(payload.starts_with(&decoded[..decoded.len().min(20)]));
    }

    #[test]
    fn run_length_literals_and_runs() {
        // 2 -> copy 3 literal bytes; 254 -> repeat next byte 3 times; 128 ends.
        let encoded = [2u8, b'a', b'b', b'c', 254, b'z', 128, b'j', b'u', b'n', b'k'];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"abczzz");
    }

    #[test]
    fn run_length_without_terminator_still_decodes() {
        let encoded = [1u8, b'h', b'i'];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"hi");
    }

    #[test]
    fn truncated_flate_keeps_what_inflated() {
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(40);
        let encoded = flate_encode(&payload);
        let decoded = flate_decode(&encoded[..encoded.len() - 12]).unwrap();
        assert!(!decoded.is_empty(), "partial inflate should still yield bytes");
        assert!(payload.starts_with(&decoded[..]));
    }

    #[test]
    fn flate_that_yields_nothing_is_still_an_error() {
        assert!(flate_decode(b"not compressed at all").is_err());
    }

    #[test]
    fn crypt_filter_passes_bytes_through() {
        let parms = DecodeParms::default();
        assert_eq!(decode("Crypt", b"already-decrypted", &parms).unwrap(), b"already-decrypted");
    }

    #[test]
    fn flate_round_trip() {
        let data = b"hello hello hello hello".to_vec();
        let encoded = flate_encode(&data);
        assert_eq!(flate_decode(&encoded).unwrap(), data);
    }

    #[test]
    fn decodes_via_dictionary_filter_chain() {
        let mut dict = Dictionary::new();
        dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
        let encoded = flate_encode(b"payload");
        assert_eq!(decode_with_dict(&dict, &encoded).unwrap(), b"payload");
    }

    #[test]
    fn ascii_hex() {
        assert_eq!(
            decode("ASCIIHexDecode", b"48 65 6C6C 6F>", &DecodeParms::default()).unwrap(),
            b"Hello"
        );
    }

    #[test]
    fn ascii85() {
        assert_eq!(
            decode("ASCII85Decode", b"87cURDZ~>", &DecodeParms::default()).unwrap(),
            b"Hello"
        );
    }

    #[test]
    fn png_up_predictor() {
        let parms = DecodeParms {
            predictor: 12,
            colors: 1,
            bits_per_component: 8,
            columns: 4,
            ..DecodeParms::default()
        };
        let raw = [
            2u8, 1, 2, 3, 4, // row 1: prev row is zeros -> 1 2 3 4
            2, 1, 1, 1, 1, // row 2: adds row 1 -> 2 3 4 5
        ];
        let inflated = flate_encode(&raw);
        let out = decode("FlateDecode", &inflated, &parms).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 2, 3, 4, 5]);
    }
}
