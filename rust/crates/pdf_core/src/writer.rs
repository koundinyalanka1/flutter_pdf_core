use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::document::PdfDocument;
use crate::error::{PdfError, Result};
use crate::lexer::{is_delimiter, is_ws};
use crate::object::{Dictionary, ObjectId, PdfObject};

pub struct PdfWriter;

impl PdfWriter {
    pub fn write_document(document: &PdfDocument, output_path: impl AsRef<Path>) -> Result<()> {
        let file = File::create(output_path)?;
        let mut writer = BufWriter::new(file);
        let bytes = Self::write_document_to_vec(document)?;
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn write_document_to_vec(document: &PdfDocument) -> Result<Vec<u8>> {
        let root = document
            .root_ref()
            .ok_or_else(|| PdfError::write("cannot write PDF without trailer /Root"))?;
        let max_object_number = document
            .objects
            .keys()
            .map(|id| id.number)
            .max()
            .unwrap_or(0);
        let mut seen_numbers = BTreeSet::new();
        for id in document.objects.keys() {
            if !seen_numbers.insert(id.number) {
                return Err(PdfError::write(format!(
                    "multiple generations for object {} are not supported by the milestone 2 writer",
                    id.number
                )));
            }
        }

        let mut out = Vec::new();
        write!(out, "%PDF-{}\n", document.version)?;
        // Keep a binary marker comment so future viewers treat the file as binary-safe.
        out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

        let mut offsets: BTreeMap<u32, (usize, u16)> = BTreeMap::new();
        for (id, object) in &document.objects {
            offsets.insert(id.number, (out.len(), id.generation));
            write!(out, "{} {} obj\n", id.number, id.generation)?;
            Self::write_object(&mut out, &object.value)?;
            out.extend_from_slice(b"\nendobj\n");
        }

        let startxref = out.len();
        out.extend_from_slice(b"xref\n");
        writeln!(out, "0 {}", max_object_number + 1)?;
        out.extend_from_slice(b"0000000000 65535 f \n");
        for object_number in 1..=max_object_number {
            if let Some((offset, generation)) = offsets.get(&object_number) {
                writeln!(out, "{offset:010} {generation:05} n ")?;
            } else {
                out.extend_from_slice(b"0000000000 00000 f \n");
            }
        }

        let trailer = Self::build_trailer(document, root, max_object_number + 1);
        out.extend_from_slice(b"trailer\n");
        Self::write_dictionary(&mut out, &trailer, None)?;
        write!(out, "\nstartxref\n{startxref}\n%%EOF\n")?;
        Ok(out)
    }

    pub fn serialize_object(object: &PdfObject) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        Self::write_object(&mut out, object)?;
        Ok(out)
    }

    fn build_trailer(document: &PdfDocument, root: ObjectId, size: u32) -> Dictionary {
        let mut trailer = Dictionary::new();
        for (key, value) in &document.xref.trailer {
            // Avoid preserving offsets into the previous file or stream-xref metadata.
            if matches!(key.as_str(), "Size" | "Prev" | "XRefStm") {
                continue;
            }
            trailer.insert(key.clone(), value.clone());
        }
        trailer.insert("Size".to_owned(), PdfObject::Integer(i64::from(size)));
        trailer.insert("Root".to_owned(), PdfObject::Reference(root));
        trailer
    }

    fn write_object(out: &mut Vec<u8>, object: &PdfObject) -> Result<()> {
        match object {
            PdfObject::Null => out.extend_from_slice(b"null"),
            PdfObject::Bool(value) => {
                out.extend_from_slice(if *value { b"true" } else { b"false" })
            }
            PdfObject::Integer(value) => write!(out, "{value}")?,
            PdfObject::Real(value) => write_real(out, *value)?,
            PdfObject::Name(name) => write_name(out, name),
            PdfObject::LiteralString(bytes) => write_literal_string(out, bytes),
            PdfObject::HexString(bytes) => write_hex_string(out, bytes),
            PdfObject::Array(items) => write_array(out, items)?,
            PdfObject::Dictionary(dict) => Self::write_dictionary(out, dict, None)?,
            PdfObject::Stream(stream) => {
                Self::write_dictionary(out, &stream.dictionary, Some(stream.data.len()))?;
                out.extend_from_slice(b"\nstream\n");
                out.extend_from_slice(&stream.data);
                out.extend_from_slice(b"\nendstream");
            }
            PdfObject::Reference(id) => write!(out, "{} {} R", id.number, id.generation)?,
        }
        Ok(())
    }

    fn write_dictionary(
        out: &mut Vec<u8>,
        dict: &Dictionary,
        stream_length: Option<usize>,
    ) -> Result<()> {
        out.extend_from_slice(b"<<");
        let mut first = true;
        for (key, value) in dict {
            if stream_length.is_some() && key == "Length" {
                continue;
            }
            if !first {
                out.push(b' ');
            }
            first = false;
            write_name(out, key);
            out.push(b' ');
            Self::write_object(out, value)?;
        }
        if let Some(length) = stream_length {
            if !first {
                out.push(b' ');
            }
            write_name(out, "Length");
            write!(out, " {length}")?;
        }
        out.extend_from_slice(b">>");
        Ok(())
    }
}

fn write_real(out: &mut Vec<u8>, value: f64) -> Result<()> {
    if !value.is_finite() {
        return Err(PdfError::write("cannot serialize non-finite real number"));
    }
    let mut text = value.to_string();
    if text.contains('e') || text.contains('E') {
        text = format!("{value:.12}");
        while text.contains('.') && text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.push('0');
        }
    }
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    out.push(b'/');
    for &byte in name.as_bytes() {
        if byte <= 0x20 || byte >= 0x7f || is_ws(byte) || is_delimiter(byte) || byte == b'#' {
            write!(out, "#{byte:02X}").expect("writing to Vec cannot fail");
        } else {
            out.push(byte);
        }
    }
}

fn write_literal_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'(');
    for &byte in bytes {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'(' => out.extend_from_slice(b"\\("),
            b')' => out.extend_from_slice(b"\\)"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            0x20..=0x7e => out.push(byte),
            _ => write!(out, "\\{byte:03o}").expect("writing to Vec cannot fail"),
        }
    }
    out.push(b')');
}

fn write_hex_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'<');
    for byte in bytes {
        write!(out, "{byte:02X}").expect("writing to Vec cannot fail");
    }
    out.push(b'>');
}

fn write_array(out: &mut Vec<u8>, items: &[PdfObject]) -> Result<()> {
    out.push(b'[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(b' ');
        }
        PdfWriter::write_object(out, item)?;
    }
    out.push(b']');
    Ok(())
}

#[allow(dead_code)]
fn _todo_markers() {
    // TODO: xref stream output.
    // TODO: object stream output.
    // TODO: incremental save.
    // TODO: stream recompression.
    // TODO: encryption-aware rewriting.
    // TODO: signed PDF preservation.
}

#[cfg(test)]
mod tests {
    use crate::document::PdfDocument;
    use crate::object::ObjectId;
    use crate::xref::parse_xref;

    use super::*;

    use crate::stream::PdfStream;

    #[test]
    fn serializes_pdf_objects() {
        let object = PdfObject::Array(vec![
            PdfObject::Null,
            PdfObject::Bool(true),
            PdfObject::Integer(-42),
            PdfObject::Real(3.25),
            PdfObject::Name("A Name".to_owned()),
            PdfObject::LiteralString(b"hi (there)\n".to_vec()),
            PdfObject::HexString(vec![0xde, 0xad]),
            PdfObject::Reference(ObjectId::new(7, 0)),
        ]);
        let bytes = PdfWriter::serialize_object(&object).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "[null true -42 3.25 /A#20Name (hi \\(there\\)\\n) <DEAD> 7 0 R]"
        );
    }

    #[test]
    fn serializes_stream_with_generated_length() {
        let mut dict = Dictionary::new();
        dict.insert("Length".to_owned(), PdfObject::Integer(999));
        let object = PdfObject::Stream(PdfStream::new(dict, b"hello".to_vec()));
        let bytes = PdfWriter::serialize_object(&object).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "<</Length 5>>\nstream\nhello\nendstream"
        );
    }

    #[test]
    fn writes_xref_offsets_to_indirect_objects() {
        let input = include_bytes!("../../../fixtures/simple.pdf");
        let document = PdfDocument::from_bytes(input).unwrap();
        let output = PdfWriter::write_document_to_vec(&document).unwrap();
        let xref = parse_xref(&output).unwrap();
        for (id, entry) in xref
            .entries
            .iter()
            .filter(|(id, entry)| id.number != 0 && entry.in_use)
        {
            let expected = format!("{} {} obj", id.number, id.generation);
            assert!(
                output[entry.offset..].starts_with(expected.as_bytes()),
                "xref offset {} did not point to {expected}",
                entry.offset
            );
        }
    }

    #[test]
    fn round_trips_tiny_fixtures() {
        for input in [
            include_bytes!("../../../fixtures/simple.pdf").as_slice(),
            include_bytes!("../../../fixtures/two_pages.pdf").as_slice(),
            include_bytes!("../../../fixtures/encrypted_marker.pdf").as_slice(),
        ] {
            let original = PdfDocument::from_bytes(input).unwrap();
            let output = PdfWriter::write_document_to_vec(&original).unwrap();
            let rewritten = PdfDocument::from_bytes(&output).unwrap();

            assert_eq!(rewritten.version, original.version);
            assert_eq!(
                rewritten.root_ref().is_some(),
                original.root_ref().is_some()
            );
            assert_eq!(rewritten.inspect().encrypted, original.inspect().encrypted);
            assert_eq!(rewritten.page_count(), original.page_count());
        }
    }
}
