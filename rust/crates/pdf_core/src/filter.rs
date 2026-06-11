//! Milestone 7: stream filters.
//!
//! Supports FlateDecode (with PNG/TIFF predictors), ASCIIHexDecode and
//! ASCII85Decode. Filter chains (`/Filter` as an array) are applied in order.

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
}

impl Default for DecodeParms {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
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
        "ASCIIHexDecode" | "AHx" => ascii_hex_decode(data),
        "ASCII85Decode" | "A85" => ascii85_decode(data),
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
    decoder
        .read_to_end(&mut out)
        .map_err(|e| PdfError::Filter(format!("FlateDecode failed: {e}")))?;
    Ok(out)
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
