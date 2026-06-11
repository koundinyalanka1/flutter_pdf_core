use std::collections::BTreeMap;

use crate::error::{PdfError, Result};
use crate::lexer::is_ws;
use crate::object::{Dictionary, ObjectId, PdfObject};
use crate::parser::Parser;

#[derive(Debug, Clone, PartialEq)]
pub struct XrefEntry {
    pub offset: usize,
    pub generation: u16,
    pub in_use: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XrefTable {
    pub entries: BTreeMap<ObjectId, XrefEntry>,
    pub trailer: Dictionary,
    pub startxref: usize,
}

pub fn parse_xref(data: &[u8]) -> Result<XrefTable> {
    let startxref = find_startxref(data)?;
    parse_xref_at(data, startxref)
}

fn find_startxref(data: &[u8]) -> Result<usize> {
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

fn parse_xref_at(data: &[u8], mut pos: usize) -> Result<XrefTable> {
    skip_ws(data, &mut pos);
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
            entries.insert(
                ObjectId::new((first + i) as u32, generation),
                XrefEntry {
                    offset,
                    generation,
                    in_use: flag == b'n',
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
    Ok(XrefTable {
        entries,
        trailer,
        startxref: find_startxref(data)?,
    })
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
        assert!(xref.entries[&ObjectId::new(1, 0)].in_use);
        assert_eq!(xref.trailer["Root"].as_ref(), Some(ObjectId::new(1, 0)));
    }
}
