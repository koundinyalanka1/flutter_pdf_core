use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::crypt::Decryptor;
use crate::derived::DerivedCache;
use crate::error::{PdfError, Result};
use crate::filter::decode_with_dict;
use crate::object::{Dictionary, IndirectObject, ObjectId, PdfObject};
use crate::parser::Parser;
use crate::stream::PdfStream;
use crate::writer::PdfWriter;
use crate::xref::{parse_xref, XrefEntry, XrefLocation, XrefTable};

#[derive(Debug, Clone)]
pub struct PdfDocument {
    pub version: String,
    pub xref: XrefTable,
    pub objects: BTreeMap<ObjectId, IndirectObject>,
    /// True when the source file was encrypted (objects are stored decrypted).
    pub was_encrypted: bool,
    /// Reported to callers/renderers when a damaged index was reconstructed.
    pub recovery_warnings: Vec<String>,
    /// What renderers derived from this document, when it may be kept: see
    /// [`DerivedCache`]. Off for a document as parsed.
    pub derived: DerivedCache,
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
        let mut recovered = false;
        // Normal files retain the indexed fast path. Recovery only reconstructs
        // structural damage; password errors never trigger a plaintext retry.
        let (mut xref, mut objects) = match parse_xref(data) {
            Ok(mut xref) => match load_indexed_objects(data, free_unwritten_entries(&mut xref)) {
                Ok(objects) => (xref, objects),
                Err(error) => {
                    recovered = true;
                    crate::recovery::reconstruct(data, Some(xref)).map_err(|_| error)?
                }
            },
            Err(error) => {
                recovered = true;
                crate::recovery::reconstruct(data, None).map_err(|_| error)?
            }
        };

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
            let decryptor = Decryptor::new(&encrypt_dict, &xref.trailer, password.as_bytes())?;
            for (id, object) in objects.iter_mut() {
                if Some(*id) == encrypt_ref {
                    continue; // the encryption dictionary itself is not encrypted
                }
                decryptor.decrypt_object(*id, &mut object.value);
            }
        }

        // Pass 2: expand object streams (PDF 1.5+ compressed objects). Each
        // container is decoded and its header read once, however many of
        // the index's objects it holds.
        let mut wanted: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (&number, entry) in &xref.entries {
            if recovered {
                break;
            }
            if let XrefLocation::InStream { stream_number, .. } = entry.location {
                wanted.entry(stream_number).or_default().push(number);
            }
        }
        let mut from_streams: Vec<IndirectObject> = Vec::new();
        for (stream_number, numbers) in wanted {
            let container_id = ObjectId::new(stream_number, 0);
            let Some(container) = objects.get(&container_id) else {
                continue;
            };
            let PdfObject::Stream(stream) = &container.value else {
                continue;
            };
            let mut container = ObjectStream::read(stream)?;
            for number in numbers {
                if let Some(obj) = container.object_numbered(number)? {
                    from_streams.push(obj);
                }
            }
        }
        for obj in from_streams {
            objects.entry(obj.id).or_insert(obj);
        }

        if recovered {
            // Without an intact xref, compressed catalogs and page dictionaries
            // can still be recovered from complete, authenticated object streams.
            expand_recovered_object_streams(&mut objects, &mut xref);
            let valid_root = xref
                .trailer
                .get("Root")
                .and_then(PdfObject::as_ref)
                .and_then(|id| objects.get(&id))
                .and_then(|o| o.value.as_dict())
                .is_some_and(|d| d.get("Type").and_then(PdfObject::as_name) == Some("Catalog"));
            if !valid_root {
                if let Some((&id, _)) = objects
                    .iter()
                    .filter(|(_, o)| {
                        o.value.as_dict().is_some_and(|d| {
                            d.get("Type").and_then(PdfObject::as_name) == Some("Catalog")
                        })
                    })
                    .max_by_key(|(id, _)| recovered_object_position(&xref, **id))
                {
                    xref.trailer.insert("Root".into(), PdfObject::Reference(id));
                }
            }
        }

        let document = Self {
            version,
            xref,
            objects,
            was_encrypted,
            recovery_warnings: if recovered {
                vec!["The PDF's damaged index was recovered. Some content may be missing; keep the original file.".into()]
            } else {
                Vec::new()
            },
            derived: DerivedCache::default(),
        };
        if recovered && document.page_count().unwrap_or(0) == 0 {
            return Err(PdfError::structure("damaged PDF has no recoverable pages"));
        }
        Ok(document)
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
                if let Some(PdfObject::Array(kids)) =
                    node.get("Kids").map(|k| self.resolve_value(k))
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
                if let Some(PdfObject::Array(kids)) =
                    node.get("Kids").map(|k| self.resolve_value(k))
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
        // Ids sort by number first, so the last has the highest. Looking at
        // every id instead made copying a document's objects one by one, as
        // split and merge do, take time in the square of their number.
        self.objects.keys().next_back().map_or(0, |id| id.number)
    }

    /// Insert a new object and return its id: numbered after every object
    /// the document has or, when one already has the largest number there
    /// is, with the lowest number none has.
    pub fn add_object(&mut self, value: PdfObject) -> ObjectId {
        let number = self
            .max_object_number()
            .checked_add(1)
            .unwrap_or_else(|| self.lowest_unused_number());
        let id = ObjectId::new(number, 0);
        self.objects.insert(id, IndirectObject { id, value });
        id
    }

    /// The lowest object number above 0 that no object has. A document
    /// holds far fewer objects than there are numbers, so there is one.
    fn lowest_unused_number(&self) -> u32 {
        let mut free = 1u32;
        for id in self.objects.keys() {
            if id.number > free {
                break;
            }
            if id.number == free {
                free = free.saturating_add(1);
            }
        }
        free
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
            recovery_warnings: Vec::new(),
            derived: DerivedCache::default(),
        }
    }
}

/// Some writers (Apple's among them) list objects they never wrote as in use
/// at offset 0, where the file header is, so no object can be there. Treat
/// those entries as free, as other readers do, rather than reading the whole
/// index as damaged.
fn free_unwritten_entries(xref: &mut XrefTable) -> &XrefTable {
    for entry in xref.entries.values_mut() {
        if matches!(entry.location, XrefLocation::InFile { offset: 0 }) {
            entry.location = XrefLocation::Free;
        }
    }
    xref
}

fn load_indexed_objects(
    data: &[u8],
    xref: &XrefTable,
) -> Result<BTreeMap<ObjectId, IndirectObject>> {
    let mut objects = BTreeMap::new();
    for (&number, entry) in &xref.entries {
        if number == 0 {
            continue;
        }
        let XrefLocation::InFile { offset } = entry.location else {
            continue;
        };
        let object = Parser::with_offset(data, offset).parse_indirect_object()?;
        if object.id != ObjectId::new(number, entry.generation) {
            return Err(PdfError::parse(
                offset,
                format!(
                    "xref points to {:?}, expected {number} {}",
                    object.id, entry.generation
                ),
            ));
        }
        objects.insert(object.id, object);
    }
    Ok(objects)
}

fn expand_recovered_object_streams(
    objects: &mut BTreeMap<ObjectId, IndirectObject>,
    xref: &mut XrefTable,
) {
    // Physical order, not object number, determines the newest complete
    // definition when a truncated incremental update has lost its index.
    let mut positions = BTreeMap::new();
    let mut containers = Vec::new();
    for (&id, object) in objects.iter() {
        let Some(offset) = xref.entries.get(&id.number).and_then(XrefEntry::offset) else {
            continue;
        };
        positions.insert(id.number, (offset, id));
        if let PdfObject::Stream(stream) = &object.value {
            if stream.dictionary.get("Type").and_then(PdfObject::as_name) == Some("ObjStm") {
                containers.push((offset, id));
            }
        }
    }
    containers.sort_unstable();
    let original_entries = xref.entries.clone();
    for (offset, container) in containers {
        let Some(PdfObject::Stream(stream)) = objects.get(&container).map(|o| &o.value) else {
            continue;
        };
        let Ok(items) = ObjectStream::read(stream).and_then(|mut stream| stream.objects()) else {
            continue;
        };
        for (index, object) in items.into_iter().enumerate() {
            if object.id.number == 0 || object.id.number == container.number {
                continue;
            }
            let allowed = offset > xref.startxref || original_entries.get(&object.id.number).is_none_or(|entry| {
                matches!(entry.location, XrefLocation::InStream { stream_number, .. } if stream_number == container.number)
            });
            if !allowed {
                continue;
            }
            if let Some(&(previous, id)) = positions.get(&object.id.number) {
                if previous >= offset {
                    continue;
                }
                objects.remove(&id);
            }
            positions.insert(object.id.number, (offset, object.id));
            xref.entries.insert(
                object.id.number,
                XrefEntry {
                    generation: 0,
                    location: XrefLocation::InStream {
                        stream_number: container.number,
                        index,
                    },
                },
            );
            objects.insert(object.id, object);
        }
    }
}

/// Catalog inference must consider the physical order of both plain and
/// compressed definitions, rather than their otherwise arbitrary object IDs.
fn recovered_object_position(xref: &XrefTable, id: ObjectId) -> (usize, usize) {
    match xref.entries.get(&id.number).map(|entry| &entry.location) {
        Some(XrefLocation::InFile { offset }) => (*offset, 0),
        Some(XrefLocation::InStream {
            stream_number,
            index,
        }) => (
            xref.entries
                .get(stream_number)
                .and_then(XrefEntry::offset)
                .unwrap_or(0),
            *index,
        ),
        _ => (0, 0),
    }
}

/// An object stream (`/Type /ObjStm`), decoded, with its header of `/N`
/// pairs read once into a table: each object's number and where the object
/// starts in the decoded bytes. Reading the header again for every object
/// the index asks for made opening a file of N compressed objects take
/// N² steps.
struct ObjectStream {
    data: Vec<u8>,
    /// Object number and start in `data`, in the header's order.
    entries: Vec<(u32, usize)>,
    /// Where each object number first appears in `entries`, built the first
    /// time an object is looked up by number.
    first_index: Option<BTreeMap<u32, usize>>,
    /// Bytes read out of `data` by the objects parsed so far.
    read: usize,
}

impl ObjectStream {
    fn read(stream: &PdfStream) -> Result<Self> {
        // A stream that counts no objects holds none, whatever an index says
        // of it. One with no count at all is not an object stream: the
        // objects an index places in it are missing, and the file still
        // opens without them, as it did before this table was kept.
        let count = stream
            .dictionary
            .get("N")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0);
        if count <= 0 {
            return Ok(Self {
                data: Vec::new(),
                entries: Vec::new(),
                first_index: None,
                read: 0,
            });
        }
        let n =
            usize::try_from(count).map_err(|_| PdfError::structure("bad object stream count"))?;
        let data = decode_with_dict(&stream.dictionary, &stream.data)?;
        let first = stream
            .dictionary
            .get("First")
            .and_then(PdfObject::as_i64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| PdfError::structure("bad object stream offset"))?;
        // Each pair takes at least four bytes ("1 0 " ends with a separator
        // the last pair may leave out), so the header holds at most
        // (First + 1) / 4 of them. A count beyond that is refused before
        // anything is read, and the table grows with the pairs actually
        // read rather than with the count the file claims.
        if first > data.len() || n > (first + 1) / 4 {
            return Err(PdfError::structure("object stream header exceeds its data"));
        }
        let mut header = Parser::new(&data[..first]);
        let mut entries = Vec::new();
        for _ in 0..n {
            let number = header
                .parse_object()?
                .as_i64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| PdfError::structure("bad compressed object number"))?;
            let offset = header
                .parse_object()?
                .as_i64()
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|v| first.checked_add(v))
                .filter(|v| *v < data.len())
                .ok_or_else(|| PdfError::structure("bad compressed object offset"))?;
            entries.push((number, offset));
        }
        Ok(Self {
            data,
            entries,
            first_index: None,
            read: 0,
        })
    }

    /// Every object in the header's order.
    fn objects(&mut self) -> Result<Vec<IndirectObject>> {
        let mut objects = Vec::new();
        for index in 0..self.entries.len() {
            objects.push(self.object(index)?);
        }
        Ok(objects)
    }

    /// The first object the header lists under `number`, if it lists one.
    fn object_numbered(&mut self, number: u32) -> Result<Option<IndirectObject>> {
        let entries = &self.entries;
        let first_index = self.first_index.get_or_insert_with(|| {
            let mut first_index = BTreeMap::new();
            for (index, &(number, _)) in entries.iter().enumerate() {
                first_index.entry(number).or_insert(index);
            }
            first_index
        });
        match first_index.get(&number) {
            Some(&index) => self.object(index).map(Some),
            None => Ok(None),
        }
    }

    fn object(&mut self, index: usize) -> Result<IndirectObject> {
        let (number, offset) = self.entries[index];
        let mut parser = Parser::with_offset(&self.data, offset);
        let value = parser.parse_object();
        // Objects in a stream do not overlap, so reading all of them reads
        // the stream about once; the lookahead past each object's end can
        // read the next two tokens again, three times at most. Offsets made
        // to overlap would have one large object parsed, and kept, once for
        // every number pointing at it.
        self.read = self
            .read
            .saturating_add(parser.furthest().saturating_sub(offset));
        if self.read > self.data.len().saturating_mul(4) {
            return Err(PdfError::structure("object stream objects overlap"));
        }
        Ok(IndirectObject {
            id: ObjectId::new(number, 0),
            value: value?,
        })
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

    /// A classic-xref PDF whose objects are numbered from 1. Each body ends
    /// however the test wants it to. `None` is listed as in use at offset 0,
    /// the way Apple's writer lists objects it never wrote.
    fn pdf_with_bodies(bodies: &[Option<&str>]) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in bodies.iter().enumerate() {
            match body {
                Some(body) => {
                    offsets.push(out.len());
                    out.extend_from_slice(format!("{} 0 obj\n{body}\n", index + 1).as_bytes());
                }
                None => offsets.push(0),
            }
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                bodies.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn apple_writer_quirks_are_read_as_an_intact_index() {
        let content = "BT ET";
        let stream_without_endobj =
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len());
        let pdf = pdf_with_bodies(&[
            Some("<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R >>\nendobj"),
            Some("<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj"),
            Some("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj"),
            Some(&stream_without_endobj),
            Some("<< /Type /StructElem /Pg 18446744073386459286 0 R /Lang (en-US) >>\nendobj"),
            None,
        ]);

        let doc = PdfDocument::from_bytes(&pdf).unwrap();
        assert!(doc.recovery_warnings.is_empty(), "{:?}", doc.recovery_warnings);
        assert_eq!(doc.page_count(), Some(1));
        assert!(matches!(doc.resolve(ObjectId::new(4, 0)), Some(PdfObject::Stream(_))));
        let element = doc.resolve(ObjectId::new(5, 0)).and_then(PdfObject::as_dict).unwrap();
        assert_eq!(element.get("Pg"), Some(&PdfObject::Null));
        assert!(element.get("Lang").is_some());
        assert!(doc.resolve(ObjectId::new(6, 0)).is_none());
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
    fn objects_an_index_places_in_a_stream_that_is_no_object_stream_are_missing() {
        use crate::filter::flate_encode;
        // The index lists object 10 as compressed in stream 4, which is the
        // page's content and has no /N. The index is wrong about object 10
        // alone: the file opens without it, as it always did.
        let content = "0 0 10 10 re f";
        let bodies = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>".to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
        let mut pdf = b"%PDF-1.5\n".to_vec();
        // Rows of /W [1 2 0]: the kind, then the offset or the stream's number.
        let mut rows = vec![0, 0, 0];
        for (index, body) in bodies.iter().enumerate() {
            rows.push(1);
            rows.extend_from_slice(&u16::try_from(pdf.len()).unwrap().to_be_bytes());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        rows.extend_from_slice(&[2, 0, 4]);
        let rows = flate_encode(&rows);
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "5 0 obj\n<</Type/XRef/Size 11/W[1 2 0]/Index[0 5 10 1]/Root 1 0 R/Filter/FlateDecode/Length {}>>\nstream\n",
                rows.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&rows);
        pdf.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );

        let doc = PdfDocument::from_bytes(&pdf).unwrap();
        assert!(
            doc.recovery_warnings.is_empty(),
            "{:?}",
            doc.recovery_warnings
        );
        assert_eq!(doc.page_count(), Some(1));
        match doc.resolve(ObjectId::new(4, 0)) {
            Some(PdfObject::Stream(stream)) => assert_eq!(stream.data, content.as_bytes()),
            other => panic!("content stream: {other:?}"),
        }
        assert!(doc.resolve(ObjectId::new(10, 0)).is_none());
    }

    #[test]
    fn new_objects_never_take_a_number_in_use() {
        // A file may number an object 4294967295, the largest number there
        // is. The number after it wrapped to 0, so every object added after
        // it, by split or merge among others, replaced the one before.
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let mut doc = PdfDocument::from_bytes(pdf).unwrap();
        let last = ObjectId::new(u32::MAX, 0);
        doc.set_object(last, PdfObject::Integer(1));
        let before = doc.objects.len();
        let first = doc.add_object(PdfObject::Integer(2));
        let second = doc.add_object(PdfObject::Integer(3));
        assert_ne!(first, second);
        assert!(
            first.number != 0 && second.number != 0,
            "{first:?} {second:?}"
        );
        assert_eq!(doc.objects.len(), before + 2);
        assert_eq!(doc.resolve(last), Some(&PdfObject::Integer(1)));
        assert_eq!(doc.resolve(first), Some(&PdfObject::Integer(2)));
        assert_eq!(doc.resolve(second), Some(&PdfObject::Integer(3)));
        assert_eq!(doc.max_object_number(), u32::MAX);
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
