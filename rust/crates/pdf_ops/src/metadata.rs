//! Milestone 6 (part 2): document metadata (the /Info dictionary).
//!
//! PDF text strings are either PDFDocEncoding (treated as Latin-1 here) or
//! UTF-16BE with a BOM. Reading handles both; writing emits ASCII directly
//! and UTF-16BE for anything else.

use pdf_core::document::PdfDocument;
use pdf_core::error::Result;
use pdf_core::object::{Dictionary, PdfObject};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub creation_date: Option<String>,
    pub mod_date: Option<String>,
}

/// Decode a PDF text string.
pub fn decode_text_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        // UTF-16BE with BOM.
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        // PDFDocEncoding ≈ Latin-1 for the printable range.
        bytes.iter().map(|&b| b as char).collect()
    }
}

/// Encode a string for storage in a PDF.
pub fn encode_text_string(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        text.as_bytes().to_vec()
    } else {
        let mut out = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    }
}

fn read_field(doc: &PdfDocument, info: &Dictionary, key: &str) -> Option<String> {
    let value = doc.resolve_value(info.get(key)?);
    match value {
        PdfObject::LiteralString(b) | PdfObject::HexString(b) => Some(decode_text_string(&b)),
        _ => None,
    }
}

/// Read the /Info dictionary.
pub fn read_metadata(doc: &PdfDocument) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();
    let Some(info) = doc
        .info_ref()
        .and_then(|id| doc.resolve(id))
        .and_then(PdfObject::as_dict)
    else {
        return meta;
    };
    meta.title = read_field(doc, info, "Title");
    meta.author = read_field(doc, info, "Author");
    meta.subject = read_field(doc, info, "Subject");
    meta.keywords = read_field(doc, info, "Keywords");
    meta.creator = read_field(doc, info, "Creator");
    meta.producer = read_field(doc, info, "Producer");
    meta.creation_date = read_field(doc, info, "CreationDate");
    meta.mod_date = read_field(doc, info, "ModDate");
    meta
}

fn apply_field(info: &mut Dictionary, key: &str, value: &Option<String>) {
    match value.as_deref() {
        None => {}
        Some("") => {
            info.remove(key);
        }
        Some(text) => {
            info.insert(
                key.to_owned(),
                PdfObject::LiteralString(encode_text_string(text)),
            );
        }
    }
}

/// Write metadata. `None` fields are left untouched; empty strings delete
/// the corresponding entry.
pub fn write_metadata(doc: &mut PdfDocument, meta: &DocumentMetadata) -> Result<()> {
    let mut info: Dictionary = doc
        .info_ref()
        .and_then(|id| doc.resolve(id))
        .and_then(PdfObject::as_dict)
        .cloned()
        .unwrap_or_default();

    apply_field(&mut info, "Title", &meta.title);
    apply_field(&mut info, "Author", &meta.author);
    apply_field(&mut info, "Subject", &meta.subject);
    apply_field(&mut info, "Keywords", &meta.keywords);
    apply_field(&mut info, "Creator", &meta.creator);
    apply_field(&mut info, "Producer", &meta.producer);
    apply_field(&mut info, "CreationDate", &meta.creation_date);
    apply_field(&mut info, "ModDate", &meta.mod_date);

    match doc.info_ref() {
        Some(id) => doc.set_object(id, PdfObject::Dictionary(info)),
        None => {
            let id = doc.add_object(PdfObject::Dictionary(info));
            doc.set_trailer_key("Info", PdfObject::Reference(id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_tree::test_support::nested_doc;

    #[test]
    fn writes_and_reads_metadata() {
        let mut doc = nested_doc(1);
        let meta = DocumentMetadata {
            title: Some("Quarterly Report".into()),
            author: Some("Koundinya".into()),
            producer: Some("flutter_pdf_core".into()),
            ..Default::default()
        };
        write_metadata(&mut doc, &meta).unwrap();
        let read = read_metadata(&doc);
        assert_eq!(read.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(read.author.as_deref(), Some("Koundinya"));
        assert_eq!(read.subject, None);

        // Survives a write/parse round trip.
        let bytes = doc.to_bytes().unwrap();
        let reread = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(
            read_metadata(&reread).title.as_deref(),
            Some("Quarterly Report")
        );
    }

    #[test]
    fn partial_updates_and_deletion() {
        let mut doc = nested_doc(1);
        write_metadata(
            &mut doc,
            &DocumentMetadata {
                title: Some("A".into()),
                author: Some("B".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // Update title only; author untouched.
        write_metadata(
            &mut doc,
            &DocumentMetadata {
                title: Some("New".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // Delete author with an empty string.
        write_metadata(
            &mut doc,
            &DocumentMetadata {
                author: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        let read = read_metadata(&doc);
        assert_eq!(read.title.as_deref(), Some("New"));
        assert_eq!(read.author, None);
    }

    #[test]
    fn unicode_round_trips_as_utf16() {
        let mut doc = nested_doc(1);
        write_metadata(
            &mut doc,
            &DocumentMetadata {
                title: Some("నివేదిక – résumé".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let bytes = doc.to_bytes().unwrap();
        let reread = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(
            read_metadata(&reread).title.as_deref(),
            Some("నివేదిక – résumé")
        );
    }
}
