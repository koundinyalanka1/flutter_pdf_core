//! Cross-reference parsing: classic tables, xref streams (PDF 1.5+),
//! `/Prev` chains for incrementally-updated files, and hybrid files
//! (`/XRefStm`).

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{PdfError, Result};
use crate::filter::decode_with_dict;
use crate::lexer::is_ws;
use crate::object::{Dictionary, ObjectId, PdfObject};
use crate::parser::Parser;

/// Where an object lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XrefLocation {
    /// Free entry.
    Free,
    /// Uncompressed object at a byte offset in the file.
    InFile { offset: usize },
    /// Compressed object inside an object stream.
    InStream { stream_number: u32, index: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct XrefEntry {
    pub location: XrefLocation,
    pub generation: u16,
}

impl XrefEntry {
    pub fn in_use(&self) -> bool {
        !matches!(self.location, XrefLocation::Free)
    }

    pub fn offset(&self) -> Option<usize> {
        match self.location {
            XrefLocation::InFile { offset } => Some(offset),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct XrefTable {
    /// Keyed by object number; entries from the newest revision win.
    pub entries: BTreeMap<u32, XrefEntry>,
    pub trailer: Dictionary,
    pub startxref: usize,
}

impl XrefTable {
    pub fn get(&self, id: ObjectId) -> Option<&XrefEntry> {
        self.entries
            .get(&id.number)
            .filter(|e| e.generation == id.generation || matches!(e.location, XrefLocation::InStream { .. }))
    }
}

const MAX_CHAIN: usize = 64;

pub fn parse_xref(data: &[u8]) -> Result<XrefTable> {
    let startxref = find_startxref(data)?;
    let mut entries: BTreeMap<u32, XrefEntry> = BTreeMap::new();
    let mut trailer = Dictionary::new();
    let mut visited: BTreeSet<usize> = BTreeSet::new();
    let mut next = Some(startxref);

    while let Some(pos) = next {
        if !visited.insert(pos) || visited.len() > MAX_CHAIN {
            break; // cycle guard
        }
        let section = parse_section(data, pos)?;
        // Hybrid file: classic table whose trailer points at an extra xref stream.
        if let Some(stm) = section.trailer.get("XRefStm").and_then(PdfObject::as_i64) {
            if stm >= 0 {
                if let Ok(extra) = parse_section(data, stm as usize) {
                    merge_section(&mut entries, &mut trailer, extra);
                }
            }
        }
        let prev = section.trailer.get("Prev").and_then(PdfObject::as_i64);
        merge_section(&mut entries, &mut trailer, section);
        next = prev.and_then(|p| usize::try_from(p).ok());
    }

    if trailer.is_empty() {
        return Err(PdfError::xref(startxref, "no trailer found"));
    }
    Ok(XrefTable {
        entries,
        trailer,
        startxref,
    })
}

struct Section {
    entries: BTreeMap<u32, XrefEntry>,
    trailer: Dictionary,
}

fn merge_section(
    entries: &mut BTreeMap<u32, XrefEntry>,
    trailer: &mut Dictionary,
    section: Section,
) {
    // Newest revision is processed first, so existing keys always win.
    for (num, entry) in section.entries {
        entries.entry(num).or_insert(entry);
    }
    for (key, value) in section.trailer {
        trailer.entry(key).or_insert(value);
    }
}

fn parse_section(data: &[u8], pos: usize) -> Result<Section> {
    let mut p = pos;
    skip_ws(data, &mut p);
    if starts_with(data, p, b"xref") {
        parse_classic_section(data, p)
    } else {
        parse_stream_section(data, p)
    }
}

// ---------------------------------------------------------------------------
// Classic `xref` tables
// ---------------------------------------------------------------------------

fn parse_classic_section(data: &[u8], mut pos: usize) -> Result<Section> {
    expect_bytes(data, &mut pos, b"xref")?;
    let mut entries = BTreeMap::new();
    loop {
        skip_ws(data, &mut pos);
        if starts_with(data, pos, b"trailer") {
            pos += b"trailer".len();
            break;
        }
        let first = read_usize(data, &mut pos)?;
        let count = read_usize(data, &mut pos)?;
        for i in 0..count {
            skip_eol(data, &mut pos);
            let offset_pos = pos;
            let offset = read_fixed_number(data, &mut pos, 10)?;
            expect_space(data, &mut pos)?;
            let generation = read_fixed_number(data, &mut pos, 5)? as u16;
            expect_space(data, &mut pos)?;
            let flag = *data
                .get(pos)
                .ok_or_else(|| PdfError::xref(pos, "missing xref flag"))?;
            pos += 1;
            skip_eol(data, &mut pos);
            if flag != b'n' && flag != b'f' {
                return Err(PdfError::xref(offset_pos, "invalid xref flag"));
            }
            let location = if flag == b'n' {
                XrefLocation::InFile { offset }
            } else {
                XrefLocation::Free
            };
            entries.insert(
                (first + i) as u32,
                XrefEntry {
                    location,
                    generation,
                },
            );
        }
    }
    skip_ws(data, &mut pos);
    let mut parser = Parser::with_offset(data, pos);
    let trailer = match parser.parse_object()? {
        PdfObject::Dictionary(dict) => dict,
        _ => return Err(PdfError::xref(pos, "trailer is not a dictionary")),
    };
    Ok(Section { entries, trailer })
}

// ---------------------------------------------------------------------------
// Xref streams (PDF 1.5+)
// ---------------------------------------------------------------------------

fn parse_stream_section(data: &[u8], pos: usize) -> Result<Section> {
    let mut parser = Parser::with_offset(data, pos);
    let object = parser.parse_indirect_object()?;
    let stream = match object.value {
        PdfObject::Stream(stream) => stream,
        _ => return Err(PdfError::xref(pos, "expected xref stream object")),
    };
    let dict = &stream.dictionary;
    if dict.get("Type").and_then(PdfObject::as_name) != Some("XRef") {
        return Err(PdfError::xref(pos, "xref stream has wrong /Type"));
    }
    let decoded = decode_with_dict(dict, &stream.data)?;

    let w: Vec<usize> = match dict.get("W") {
        Some(PdfObject::Array(items)) => items
            .iter()
            .map(|o| o.as_i64().unwrap_or(0).max(0) as usize)
            .collect(),
        _ => return Err(PdfError::xref(pos, "xref stream missing /W")),
    };
    if w.len() < 3 {
        return Err(PdfError::xref(pos, "/W must have 3 entries"));
    }
    let row_width: usize = w.iter().sum();
    if row_width == 0 {
        return Err(PdfError::xref(pos, "/W is all zeros"));
    }

    let size = dict
        .get("Size")
        .and_then(PdfObject::as_i64)
        .ok_or_else(|| PdfError::xref(pos, "xref stream missing /Size"))?;
    let index_pairs: Vec<(u32, usize)> = match dict.get("Index") {
        Some(PdfObject::Array(items)) => items
            .chunks(2)
            .filter_map(|pair| {
                let first = pair.first()?.as_i64()?;
                let count = pair.get(1)?.as_i64()?;
                Some((first as u32, count as usize))
            })
            .collect(),
        _ => vec![(0, size.max(0) as usize)],
    };

    let mut entries = BTreeMap::new();
    let mut row = 0usize;
    for (first, count) in index_pairs {
        for i in 0..count {
            let start = row * row_width;
            row += 1;
            let Some(bytes) = decoded.get(start..start + row_width) else {
                break; // tolerate truncated xref stream data
            };
            let mut cursor = 0usize;
            let read_field = |width: usize, cursor: &mut usize, default: u64| -> u64 {
                if width == 0 {
                    return default;
                }
                let mut value = 0u64;
                for &b in &bytes[*cursor..*cursor + width] {
                    value = (value << 8) | b as u64;
                }
                *cursor += width;
                value
            };
            let kind = read_field(w[0], &mut cursor, 1);
            let f2 = read_field(w[1], &mut cursor, 0);
            let f3 = read_field(w[2], &mut cursor, 0);
            let number = first + i as u32;
            let entry = match kind {
                0 => XrefEntry {
                    location: XrefLocation::Free,
                    generation: f3 as u16,
                },
                1 => XrefEntry {
                    location: XrefLocation::InFile {
                        offset: f2 as usize,
                    },
                    generation: f3 as u16,
                },
                2 => XrefEntry {
                    location: XrefLocation::InStream {
                        stream_number: f2 as u32,
                        index: f3 as usize,
                    },
                    generation: 0,
                },
                _ => continue, // spec: treat unknown types as null references
            };
            entries.insert(number, entry);
        }
    }

    // The xref stream's dictionary doubles as the trailer.
    let trailer: Dictionary = dict
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "Type" | "W" | "Index" | "Filter" | "DecodeParms" | "DP" | "Length"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok(Section { entries, trailer })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn find_startxref(data: &[u8]) -> Result<usize> {
    let marker = b"startxref";
    let idx = data
        .windows(marker.len())
        .rposition(|w| w == marker)
        .ok_or_else(|| PdfError::xref(data.len(), "missing startxref"))?;
    let mut pos = idx + marker.len();
    while pos < data.len() && is_ws(data[pos]) {
        pos += 1;
    }
    let start = pos;
    while pos < data.len() && data[pos].is_ascii_digit() {
        pos += 1;
    }
    std::str::from_utf8(&data[start..pos])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| PdfError::xref(start, "invalid startxref offset"))
}

fn starts_with(data: &[u8], pos: usize, prefix: &[u8]) -> bool {
    data.get(pos..pos + prefix.len()) == Some(prefix)
}

fn expect_bytes(data: &[u8], pos: &mut usize, expected: &[u8]) -> Result<()> {
    if starts_with(data, *pos, expected) {
        *pos += expected.len();
        Ok(())
    } else {
        Err(PdfError::xref(
            *pos,
            format!("expected {}", String::from_utf8_lossy(expected)),
        ))
    }
}

fn skip_ws(data: &[u8], pos: &mut usize) {
    while *pos < data.len() && is_ws(data[*pos]) {
        *pos += 1;
    }
}

fn skip_eol(data: &[u8], pos: &mut usize) {
    while *pos < data.len() && (data[*pos] == b' ' || data[*pos] == b'\t') {
        *pos += 1;
    }
    if *pos < data.len() && data[*pos] == b'\r' {
        *pos += 1;
    }
    if *pos < data.len() && data[*pos] == b'\n' {
        *pos += 1;
    }
}

fn expect_space(data: &[u8], pos: &mut usize) -> Result<()> {
    if *pos < data.len() && (data[*pos] == b' ' || data[*pos] == b'\t') {
        while *pos < data.len() && (data[*pos] == b' ' || data[*pos] == b'\t') {
            *pos += 1;
        }
        Ok(())
    } else {
        Err(PdfError::xref(*pos, "expected space"))
    }
}

fn read_usize(data: &[u8], pos: &mut usize) -> Result<usize> {
    skip_ws(data, pos);
    let start = *pos;
    while *pos < data.len() && data[*pos].is_ascii_digit() {
        *pos += 1;
    }
    std::str::from_utf8(&data[start..*pos])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| PdfError::xref(start, "expected integer"))
}

fn read_fixed_number(data: &[u8], pos: &mut usize, width: usize) -> Result<usize> {
    let start = *pos;
    let end = start + width;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| PdfError::xref(start, "truncated xref entry"))?;
    if !bytes.iter().all(u8::is_ascii_digit) {
        return Err(PdfError::xref(start, "invalid xref number"));
    }
    *pos = end;
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| PdfError::xref(start, "invalid xref number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_classic_xref_and_trailer() {
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let xref = parse_xref(pdf).unwrap();
        assert!(xref.entries[&1].in_use());
        assert_eq!(xref.trailer["Root"].as_ref(), Some(ObjectId::new(1, 0)));
    }

    #[test]
    fn newest_revision_wins_in_prev_chain() {
        // Build a tiny two-revision file by hand.
        let mut pdf: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let obj1_v1 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n(first)\nendobj\n");
        let xref1 = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{obj1_v1:010} 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\nstartxref\n{xref1}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let obj1_v2 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n(second)\nendobj\n");
        let xref2 = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 1\n0000000000 65535 f \n1 1\n{obj1_v2:010} 00000 n \ntrailer\n<</Size 2/Root 1 0 R/Prev {xref1}>>\nstartxref\n{xref2}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let xref = parse_xref(&pdf).unwrap();
        assert_eq!(
            xref.entries[&1].offset(),
            Some(obj1_v2),
            "latest revision should win"
        );
    }

    #[test]
    fn parses_xref_stream_section() {
        use crate::filter::flate_encode;
        // Three entries: free, in-file at 9, compressed in stream 5 index 2.
        let rows: Vec<u8> = vec![
            0, 0, 0, 255, 255, // type 0
            1, 0, 9, 0, 0, // type 1 offset 9
            2, 0, 5, 0, 2, // type 2 stream 5 idx 2
        ];
        let compressed = flate_encode(&rows);
        let mut pdf: Vec<u8> = b"%PDF-1.5\n".to_vec();
        let xref_pos = pdf.len();
        pdf.extend_from_slice(
            format!(
                "7 0 obj\n<</Type/XRef/Size 3/W[1 2 2]/Root 1 0 R/Filter/FlateDecode/Length {}>>\nstream\n",
                compressed.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&compressed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF\n").as_bytes());

        let xref = parse_xref(&pdf).unwrap();
        assert_eq!(xref.entries[&0].location, XrefLocation::Free);
        assert_eq!(xref.entries[&1].location, XrefLocation::InFile { offset: 9 });
        assert_eq!(
            xref.entries[&2].location,
            XrefLocation::InStream {
                stream_number: 5,
                index: 2
            }
        );
        assert_eq!(xref.trailer["Root"].as_ref(), Some(ObjectId::new(1, 0)));
    }
}
