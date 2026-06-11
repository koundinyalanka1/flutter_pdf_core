//! Milestone 9 (part 2): AI-ready JSON / NDJSON export.
//!
//! The JSON form is one self-describing document (metadata + pages +
//! chunks). The NDJSON form emits one header line followed by one line per
//! chunk — ideal for streaming into local AI pipelines and embedding jobs.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_ops::metadata::{read_metadata, DocumentMetadata};
use pdf_text::extractor::extract_all_pages;
use serde::{Deserialize, Serialize};

use crate::chunker::{chunk_pages, Chunk, ChunkOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageText {
    pub page: usize,
    pub text: String,
    pub char_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentExport {
    pub schema: String,
    pub metadata: DocumentMetadata,
    pub page_count: usize,
    pub pages: Vec<PageText>,
    pub chunks: Vec<Chunk>,
}

pub const SCHEMA: &str = "flutter_pdf_core/export/v1";

/// Build the full export model.
pub fn export_document(doc: &PdfDocument, options: ChunkOptions) -> Result<DocumentExport> {
    let pages = extract_all_pages(doc)?;
    let chunks = chunk_pages(&pages, options);
    Ok(DocumentExport {
        schema: SCHEMA.to_owned(),
        metadata: read_metadata(doc),
        page_count: pages.len(),
        pages: pages
            .into_iter()
            .enumerate()
            .map(|(page, text)| PageText {
                page,
                char_count: text.chars().count(),
                text,
            })
            .collect(),
        chunks,
    })
}

/// Single JSON document.
pub fn to_json(doc: &PdfDocument, options: ChunkOptions) -> Result<String> {
    let export = export_document(doc, options)?;
    serde_json::to_string_pretty(&export).map_err(|e| PdfError::Structure(e.to_string()))
}

#[derive(Debug, Serialize)]
struct NdjsonHeader<'a> {
    schema: &'a str,
    kind: &'a str,
    metadata: &'a DocumentMetadata,
    page_count: usize,
    chunk_count: usize,
}

#[derive(Debug, Serialize)]
struct NdjsonChunk<'a> {
    kind: &'a str,
    #[serde(flatten)]
    chunk: &'a Chunk,
}

/// NDJSON: header line, then one line per chunk.
pub fn to_ndjson(doc: &PdfDocument, options: ChunkOptions) -> Result<String> {
    let export = export_document(doc, options)?;
    let mut out = String::new();
    let header = NdjsonHeader {
        schema: SCHEMA,
        kind: "document",
        metadata: &export.metadata,
        page_count: export.page_count,
        chunk_count: export.chunks.len(),
    };
    out.push_str(
        &serde_json::to_string(&header).map_err(|e| PdfError::Structure(e.to_string()))?,
    );
    out.push('\n');
    for chunk in &export.chunks {
        let line = NdjsonChunk {
            kind: "chunk",
            chunk,
        };
        out.push_str(
            &serde_json::to_string(&line).map_err(|e| PdfError::Structure(e.to_string()))?,
        );
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_core::object::{Dictionary, ObjectId, PdfObject};
    use pdf_core::stream::PdfStream;

    fn sample_doc() -> PdfDocument {
        // Reuse the extractor's builder shape: one page, simple text.
        let content: &[u8] = b"BT /F1 12 Tf 72 720 Td (AI export test text.) Tj ET";
        let mut doc = PdfDocument::new_empty("1.7");
        let mut font = Dictionary::new();
        font.insert("Type".into(), PdfObject::Name("Font".into()));
        font.insert("Subtype".into(), PdfObject::Name("Type1".into()));
        let font_id = doc.add_object(PdfObject::Dictionary(font));
        let mut fonts = Dictionary::new();
        fonts.insert("F1".into(), PdfObject::Reference(font_id));
        let mut resources = Dictionary::new();
        resources.insert("Font".into(), PdfObject::Dictionary(fonts));
        let mut sd = Dictionary::new();
        sd.insert("Length".into(), PdfObject::Integer(content.len() as i64));
        let content_id = doc.add_object(PdfObject::Stream(PdfStream::new(sd, content.to_vec())));
        let pages_id = ObjectId::new(50, 0);
        let mut page = Dictionary::new();
        page.insert("Type".into(), PdfObject::Name("Page".into()));
        page.insert("Parent".into(), PdfObject::Reference(pages_id));
        page.insert("Resources".into(), PdfObject::Dictionary(resources));
        page.insert("Contents".into(), PdfObject::Reference(content_id));
        let page_id = doc.add_object(PdfObject::Dictionary(page));
        let mut pages = Dictionary::new();
        pages.insert("Type".into(), PdfObject::Name("Pages".into()));
        pages.insert(
            "Kids".into(),
            PdfObject::Array(vec![PdfObject::Reference(page_id)]),
        );
        pages.insert("Count".into(), PdfObject::Integer(1));
        doc.set_object(pages_id, PdfObject::Dictionary(pages));
        let mut catalog = Dictionary::new();
        catalog.insert("Type".into(), PdfObject::Name("Catalog".into()));
        catalog.insert("Pages".into(), PdfObject::Reference(pages_id));
        let catalog_id = doc.add_object(PdfObject::Dictionary(catalog));
        doc.set_trailer_key("Root", PdfObject::Reference(catalog_id));
        pdf_ops::metadata::write_metadata(
            &mut doc,
            &pdf_ops::metadata::DocumentMetadata {
                title: Some("Export sample".into()),
                ..Default::default()
            },
        )
        .unwrap();
        doc
    }

    #[test]
    fn json_export_contains_text_and_metadata() {
        let doc = sample_doc();
        let json = to_json(&doc, ChunkOptions::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["page_count"], 1);
        assert_eq!(value["metadata"]["title"], "Export sample");
        assert!(value["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("AI export test text."));
        assert!(value["chunks"].as_array().unwrap().len() >= 1);
    }

    #[test]
    fn ndjson_has_header_plus_chunk_lines() {
        let doc = sample_doc();
        let ndjson = to_ndjson(&doc, ChunkOptions::default()).unwrap();
        let lines: Vec<&str> = ndjson.trim_end().lines().collect();
        assert!(lines.len() >= 2);
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["kind"], "document");
        let chunk: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(chunk["kind"], "chunk");
        assert!(chunk["text"].as_str().unwrap().contains("AI export"));
    }
}
