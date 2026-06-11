use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::crypt::Decryptor;
use crate::error::{PdfError, Result};
use crate::filter::decode_with_dict;
use crate::object::{Dictionary, IndirectObject, ObjectId, PdfObject};
use crate::parser::Parser;
use crate::stream::PdfStream;
use crate::writer::PdfWriter;
use crate::xref::{parse_xref, XrefLocation, XrefTable};

#[derive(Debug, Clone)]
pub struct PdfDocument {
    pub version: String,
    pub xref: XrefTable,
    pub objects: BTreeMap<ObjectId, IndirectObject>,
    /// True when the source file was encrypted (objects are stored decrypted).
    pub was_encrypted: bool,
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

    pub fn from_path_with_password(path: impl AsRef<Path>, password: &str) -> Result<Self> {
        let data = fs::read(path)?;
        Self::from_bytes_with_password(&data, password)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::from_bytes_with_password(data, "")
    }

    pub fn from_bytes_with_password(data: &[u8], password: &str) -> Result<Self> {
        let version = detect_header(data)?;
        let xref = parse_xref(data)?;
        let mut objects = BTreeMap::new();

        // Pass 1: load uncompressed objects.
        for (&number, entry) in &xref.entries {
            if number == 0 {
                continue;
            }
            let XrefLocation::InFile { offset } = entry.location else {
                continue;
            };
            let mut parser = Parser::with_offset(data, offset);
            let obj = parser.parse_indirect_object()?;
            if obj.id.number != number {
                return Err(PdfError::parse(
                    offset,
                    format!(
                        "xref points to object {}, expected {number}",
                        obj.id.number
                    ),
                ));
            }
            objects.insert(obj.id, obj);
        }

        // Decrypt if needed (object streams are decrypted as whole streams,
        // so this must happen before pass 2).
        let mut was_encrypted = false;
        if let Some(encrypt_obj) = xref.trailer.get("Encrypt") {
            was_encrypted = true;
            let encrypt_dict = match encrypt_obj {
                PdfObject::Reference(id) => objects
                    .get(id)
                    .and_then(|o| o.value.as_dict())
                    .cloned()
                    .ok_or_else(|| PdfError::structure("missing /Encrypt dictionary"))?,
                PdfObject::Dictionary(d) => d.clone(),
                _ => return Err(PdfError::structure("invalid /Encrypt entry")),
            };
            let encrypt_ref = encrypt_obj.as_ref();
            let decryptor =
                Decryptor::new(&encrypt_dict, &xref.trailer, password.as_bytes())?;
            for (id, object) in objects.iter_mut() {
                if Some(*id) == encrypt_ref {
                    continue; // the encryption dictionary itself is not encrypted
                }
                decryptor.decrypt_object(*id, &mut object.value);
            }
        }

        // Pass 2: expand object streams (PDF 1.5+ compressed objects).
        let mut from_streams: Vec<IndirectObject> = Vec::new();
        for (&number, entry) in &xref.entries {
            let XrefLocation::InStream { stream_number, .. } = entry.location else {
                continue;
            };
            let container_id = ObjectId::new(stream_number, 0);
            let Some(container) = objects.get(&container_id) else {
                continue;
            };
            let PdfObject::Stream(stream) = &container.value else {
                continue;
            };
            if let Some(obj) = extract_from_object_stream(stream, number)? {
                from_streams.push(obj);
            }
        }
        for obj in from_streams {
            objects.entry(obj.id).or_insert(obj);
        }

        Ok(Self {
            version,
            xref,
            objects,
            was_encrypted,
        })
    }

    pub fn save_as(&self, output_path: impl AsRef<Path>) -> Result<()> {
        PdfWriter::write_document(self, output_path)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        PdfWriter::write_document_to_vec(self)
    }

    pub fn inspect(&self) -> PdfInspect {
        let mut trailer_keys = self.xref.trailer.keys().cloned().collect::<Vec<_>>();
        trailer_keys.sort();
        let root = self.root_ref();
        PdfInspect {
            version: self.version.clone(),
            encrypted: self.was_encrypted,
            object_count: self.objects.len(),
            page_count: self.page_count(),
            trailer_keys,
            root,
        }
    }

    pub fn root_ref(&self) -> Option<ObjectId> {
        self.xref.trailer.get("Root").and_then(PdfObject::as_ref)
    }

    pub fn catalog(&self) -> Option<&Dictionary> {
        self.resolve(self.root_ref()?)?.as_dict()
    }

    pub fn info_ref(&self) -> Option<ObjectId> {
        self.xref.trailer.get("Info").and_then(PdfObject::as_ref)
    }

    /// Page count derived from walking the page tree (falls back to the
    /// root node's /Count when the tree cannot be walked).
    pub fn page_count(&self) -> Option<u32> {
        if let Some(pages) = self.collect_page_ids() {
            return Some(pages.len() as u32);
        }
        let root = self.catalog()?;
        let pages = self.resolve_dict(root.get("Pages")?)?;
        pages
            .get("Count")?
            .as_i64()
            .and_then(|count| u32::try_from(count).ok())
    }

    /// Flat, ordered list of page object ids (depth-first page-tree walk
    /// with cycle protection). Returns None when there is no page tree.
    pub fn collect_page_ids(&self) -> Option<Vec<ObjectId>> {
        let root = self.catalog()?;
        let pages_ref = root.get("Pages")?.as_ref()?;
        let mut out = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        self.walk_pages(pages_ref, &mut visited, &mut out, 0);
        Some(out)
    }

    fn walk_pages(
        &self,
        node_ref: ObjectId,
        visited: &mut std::collections::BTreeSet<ObjectId>,
        out: &mut Vec<ObjectId>,
        depth: usize,
    ) {
        if depth > 256 || !visited.insert(node_ref) {
            return;
        }
        let Some(node) = self.resolve(node_ref).and_then(PdfObject::as_dict) else {
            return;
        };
        match node.get("Type").and_then(PdfObject::as_name) {
            Some("Pages") => {
                if let Some(PdfObject::Array(kids)) = node.get("Kids").map(|k| self.resolve_value(k))
                {
                    for kid in kids {
                        if let Some(kid_ref) = kid.as_ref() {
                            self.walk_pages(kid_ref, visited, out, depth + 1);
                        }
                    }
                }
            }
            // Treat nodes without /Type but with /Kids as intermediate nodes.
            None if node.contains_key("Kids") => {
                if let Some(PdfObject::Array(kids)) = node.get("Kids").map(|k| self.resolve_value(k))
                {
                    for kid in kids {
                        if let Some(kid_ref) = kid.as_ref() {
                            self.walk_pages(kid_ref, visited, out, depth + 1);
                        }
                    }
                }
            }
            _ => out.push(node_ref),
        }
    }

    /// Resolve a reference, following chains of references (with a guard).
    pub fn resolve(&self, id: ObjectId) -> Option<&PdfObject> {
        let mut current = self.objects.get(&id).map(|obj| &obj.value)?;
        for _ in 0..32 {
            match current {
                PdfObject::Reference(next) => {
                    current = self.objects.get(next).map(|obj| &obj.value)?;
                }
                other => return Some(other),
            }
        }
        None
    }

    /// Resolve an object that may itself be a reference.
    pub fn resolve_value<'a>(&'a self, object: &'a PdfObject) -> PdfObject {
        match object {
            PdfObject::Reference(id) => self.resolve(*id).cloned().unwrap_or(PdfObject::Null),
            other => other.clone(),
        }
    }

    /// Resolve to a dictionary (through references).
    pub fn resolve_dict<'a>(&'a self, object: &'a PdfObject) -> Option<&'a Dictionary> {
        match object {
            PdfObject::Reference(id) => self.resolve(*id)?.as_dict(),
            other => other.as_dict(),
        }
    }

    /// Decoded (defiltered) data for a stream object.
    pub fn stream_data(&self, stream: &PdfStream) -> Result<Vec<u8>> {
        decode_with_dict(&stream.dictionary, &stream.data)
    }

    // -- Mutation helpers (milestones 3+) -----------------------------------

    pub fn max_object_number(&self) -> u32 {
        self.objects.keys().map(|id| id.number).max().unwrap_or(0)
    }

    /// Insert a new object and return its id.
    pub fn add_object(&mut self, value: PdfObject) -> ObjectId {
        let id = ObjectId::new(self.max_object_number() + 1, 0);
        self.objects.insert(id, IndirectObject { id, value });
        id
    }

    /// Replace (or insert) the object stored under `id`.
    pub fn set_object(&mut self, id: ObjectId, value: PdfObject) {
        self.objects.insert(id, IndirectObject { id, value });
    }

    pub fn set_trailer_key(&mut self, key: &str, value: PdfObject) {
        self.xref.trailer.insert(key.to_owned(), value);
    }

    /// Drop every object not reachable from the trailer's /Root and /Info.
    pub fn garbage_collect(&mut self) {
        let mut reachable = std::collections::BTreeSet::new();
        let mut queue: Vec<ObjectId> = Vec::new();
        for key in ["Root", "Info"] {
            if let Some(id) = self.xref.trailer.get(key).and_then(PdfObject::as_ref) {
                queue.push(id);
            }
        }
        while let Some(id) = queue.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(obj) = self.objects.get(&id) {
                collect_refs(&obj.value, &mut queue);
            }
        }
        self.objects.retain(|id, _| reachable.contains(id));
    }

    /// Build a fresh, minimal document (no objects, version inherited).
    pub fn new_empty(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            xref: XrefTable {
                entries: BTreeMap::new(),
                trailer: Dictionary::new(),
                startxref: 0,
            },
            objects: BTreeMap::new(),
            was_encrypted: false,
        }
    }
}

fn collect_refs(object: &PdfObject, out: &mut Vec<ObjectId>) {
    match object {
        PdfObject::Reference(id) => out.push(*id),
        PdfObject::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        PdfObject::Dictionary(dict) => {
            for value in dict.values() {
                collect_refs(value, out);
            }
        }
        PdfObject::Stream(stream) => {
            for value in stream.dictionary.values() {
                collect_refs(value, out);
            }
        }
        _ => {}
    }
}

/// Pull one object out of an object stream (`/Type /ObjStm`).
fn extract_from_object_stream(
    stream: &PdfStream,
    wanted_number: u32,
) -> Result<Option<IndirectObject>> {
    let dict = &stream.dictionary;
    let n = dict.get("N").and_then(PdfObject::as_i64).unwrap_or(0).max(0) as usize;
    let first = dict
        .get("First")
        .and_then(PdfObject::as_i64)
        .unwrap_or(0)
        .max(0) as usize;
    let data = decode_with_dict(dict, &stream.data)?;

    // Header: N pairs of "object-number offset".
    let mut header = Parser::new(&data);
    let mut pairs = Vec::with_capacity(n);
    for _ in 0..n {
        let num = match header.parse_object()? {
            PdfObject::Integer(v) => v,
            _ => return Err(PdfError::structure("bad object stream header")),
        };
        let off = match header.parse_object()? {
            PdfObject::Integer(v) => v,
            _ => return Err(PdfError::structure("bad object stream header")),
        };
        pairs.push((num as u32, off.max(0) as usize));
    }
    for (num, off) in pairs {
        if num == wanted_number {
            let mut parser = Parser::with_offset(&data, first + off);
            let value = parser.parse_object()?;
            return Ok(Some(IndirectObject {
                id: ObjectId::new(num, 0),
                value,
            }));
        }
    }
    Ok(None)
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
    }

    #[test]
    fn no_panic_on_malformed_pdf() {
        let err = PdfDocument::from_bytes(b"%PDF-1.4\nxref\n").unwrap_err();
        assert!(err.to_string().contains("startxref"));
    }

    #[test]
    fn loads_objects_from_object_streams() {
        use crate::filter::flate_encode;
        // Object stream containing 2 objects: "10 0" -> (hi), "11 0" -> 42.
        let body = b"(hi) 42";
        let header = b"10 0 11 5 ";
        let mut payload = header.to_vec();
        payload.extend_from_slice(body);
        let compressed = flate_encode(&payload);

        let mut pdf: Vec<u8> = b"%PDF-1.5\n".to_vec();
        let objstm_pos = pdf.len();
        pdf.extend_from_slice(
            format!(
                "3 0 obj\n<</Type/ObjStm/N 2/First {}/Filter/FlateDecode/Length {}>>\nstream\n",
                header.len(),
                compressed.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&compressed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let cat_pos = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<</Type/Catalog>>\nendobj\n");

        // Xref stream covering: 0 free, 1 in-file, 3 in-file, 10/11 in stream 3.
        let rows: Vec<u8> = vec![
            0,
            0,
            0, // 0: free
            1,
            (cat_pos >> 8) as u8,
            cat_pos as u8, // 1: catalog
            1,
            (objstm_pos >> 8) as u8,
            objstm_pos as u8, // 3: objstm
            2,
            0,
            3, // 10: in stream 3, index 0
            2,
            0,
            3, // 11: in stream 3, index 1
        ];
        let xref_data = flate_encode(&rows);
        let xref_pos = pdf.len();
        pdf.extend_from_slice(
            format!(
                "7 0 obj\n<</Type/XRef/Size 12/W[1 2 0]/Index[0 1 1 1 3 1 10 2]/Root 1 0 R/Filter/FlateDecode/Length {}>>\nstream\n",
                xref_data.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&xref_data);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF\n").as_bytes());

        let doc = PdfDocument::from_bytes(&pdf).unwrap();
        assert_eq!(
            doc.resolve(ObjectId::new(10, 0)),
            Some(&PdfObject::LiteralString(b"hi".to_vec()))
        );
        assert_eq!(
            doc.resolve(ObjectId::new(11, 0)),
            Some(&PdfObject::Integer(42))
        );
    }

    #[test]
    fn gc_drops_unreachable_objects() {
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let mut doc = PdfDocument::from_bytes(pdf).unwrap();
        let orphan = doc.add_object(PdfObject::Integer(7));
        assert!(doc.objects.contains_key(&orphan));
        doc.garbage_collect();
        assert!(!doc.objects.contains_key(&orphan));
        assert!(doc.page_count().unwrap() >= 1);
    }
}
