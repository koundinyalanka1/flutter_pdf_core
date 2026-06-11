use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, IndirectObject, ObjectId, PdfObject};
use crate::parser::Parser;
use crate::writer::PdfWriter;
use crate::xref::{parse_xref, XrefTable};

#[derive(Debug, Clone)]
pub struct PdfDocument {
    pub version: String,
    pub xref: XrefTable,
    pub objects: BTreeMap<ObjectId, IndirectObject>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PdfInspect {
    pub version: String,
    pub encrypted: bool,
    pub object_count: usize,
    pub page_count: Option<u32>,
    pub trailer_keys: Vec<String>,
    pub root: Option<ObjectId>,
}

impl PdfDocument {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let data = fs::read(path)?;
        Self::from_bytes(&data)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let version = detect_header(data)?;
        let xref = parse_xref(data)?;
        let mut objects = BTreeMap::new();
        for (id, entry) in &xref.entries {
            if !entry.in_use || id.number == 0 {
                continue;
            }
            let mut parser = Parser::with_offset(data, entry.offset);
            match parser.parse_indirect_object() {
                Ok(obj) if obj.id == *id => {
                    objects.insert(*id, obj);
                }
                Ok(obj) => {
                    return Err(PdfError::parse(
                        entry.offset,
                        format!("xref points to object {:?}, expected {:?}", obj.id, id),
                    ));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(Self {
            version,
            xref,
            objects,
        })
    }

    pub fn save_as(&self, output_path: impl AsRef<Path>) -> Result<()> {
        PdfWriter::write_document(self, output_path)
    }

    pub fn inspect(&self) -> PdfInspect {
        let mut trailer_keys = self.xref.trailer.keys().cloned().collect::<Vec<_>>();
        trailer_keys.sort();
        let root = self.root_ref();
        PdfInspect {
            version: self.version.clone(),
            encrypted: self.xref.trailer.contains_key("Encrypt"),
            object_count: self.objects.len(),
            page_count: self.page_count(),
            trailer_keys,
            root,
        }
    }

    pub fn root_ref(&self) -> Option<ObjectId> {
        self.xref.trailer.get("Root").and_then(PdfObject::as_ref)
    }

    pub fn page_count(&self) -> Option<u32> {
        let root = self.resolve(self.root_ref()?)?.as_dict()?;
        let pages_ref = root.get("Pages")?.as_ref()?;
        self.page_count_from_pages(pages_ref)
    }

    fn page_count_from_pages(&self, pages_ref: ObjectId) -> Option<u32> {
        let pages = self.resolve(pages_ref)?.as_dict()?;
        // Milestone 1 only follows the root /Pages node and trusts /Count.
        pages
            .get("Count")?
            .as_i64()
            .and_then(|count| u32::try_from(count).ok())
    }

    pub fn resolve(&self, id: ObjectId) -> Option<&PdfObject> {
        self.objects.get(&id).map(|obj| &obj.value)
    }
}

pub fn detect_header(data: &[u8]) -> Result<String> {
    let prefix = b"%PDF-";
    let search_len = data.len().min(1024);
    let offset = data[..search_len]
        .windows(prefix.len())
        .position(|w| w == prefix)
        .ok_or(PdfError::MissingHeader)?;
    let start = offset + prefix.len();
    let mut end = start;
    while end < data.len() && matches!(data[end], b'0'..=b'9' | b'.') {
        end += 1;
    }
    if end == start {
        return Err(PdfError::parse(offset, "missing PDF version"));
    }
    Ok(String::from_utf8_lossy(&data[start..end]).into_owned())
}

#[allow(dead_code)]
fn _todo_markers(_: &Dictionary) {
    // TODO: xref streams.
    // TODO: object streams.
    // TODO: encryption.
    // TODO: incremental updates.
    // TODO: compressed object streams.
    // TODO: full text extraction.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_header() {
        assert_eq!(detect_header(b"junk\n%PDF-1.7\n").unwrap(), "1.7");
        assert!(detect_header(b"not a pdf").is_err());
    }

    #[test]
    fn inspects_simple_pdf_page_count() {
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let doc = PdfDocument::from_bytes(pdf).unwrap();
        let info = doc.inspect();
        assert_eq!(info.version, "1.4");
        assert_eq!(info.root, Some(ObjectId::new(1, 0)));
        assert_eq!(info.page_count, Some(1));
        assert!(!info.encrypted);
    }

    #[test]
    fn inspects_additional_tiny_fixtures() {
        let two_pages =
            PdfDocument::from_bytes(include_bytes!("../../../fixtures/two_pages.pdf")).unwrap();
        assert_eq!(two_pages.inspect().page_count, Some(2));

        let encrypted =
            PdfDocument::from_bytes(include_bytes!("../../../fixtures/encrypted_marker.pdf"))
                .unwrap();
        assert!(encrypted.inspect().encrypted);
        assert_eq!(encrypted.inspect().page_count, Some(0));
    }

    #[test]
    fn no_panic_on_malformed_pdf() {
        let err = PdfDocument::from_bytes(b"%PDF-1.4\nxref\n").unwrap_err();
        assert!(err.to_string().contains("startxref"));
    }
}
