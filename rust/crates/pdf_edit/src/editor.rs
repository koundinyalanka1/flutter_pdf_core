//! A document being edited, and how it is saved: an incremental update
//! appended to the original bytes, so everything already in the file,
//! signatures included, stays exactly as it was.

use std::collections::BTreeSet;
use std::io::Write;

use pdf_core::crypt::Decryptor;
use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_core::writer::PdfWriter;

pub struct Editor {
    pub doc: PdfDocument,
    original: Vec<u8>,
    password: String,
    changed: BTreeSet<ObjectId>,
    /// The number the next object added gets. It may pass the largest
    /// object number there is, 4294967295, rather than wrap onto the
    /// file's own objects; then saving refuses.
    next: u64,
    /// The file's key, for what an encrypted file's update adds.
    crypt: Option<(Decryptor, Option<ObjectId>)>,
}

impl Editor {
    pub fn open(bytes: Vec<u8>, password: &str) -> Result<Self> {
        let doc = PdfDocument::from_bytes_with_password(&bytes, password)?;
        let size = doc
            .xref
            .trailer
            .get("Size")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0)
            .clamp(0, 1 << 32) as u64;
        let next = size.max(u64::from(doc.max_object_number()) + 1).max(1);
        let crypt = match doc.xref.trailer.get("Encrypt") {
            Some(entry) if doc.was_encrypted => {
                let dict = doc
                    .resolve_dict(entry)
                    .cloned()
                    .ok_or_else(|| PdfError::Structure("missing /Encrypt dictionary".into()))?;
                let decryptor = Decryptor::new(&dict, &doc.xref.trailer, password.as_bytes())?;
                Some((decryptor, entry.as_ref()))
            }
            _ => None,
        };
        Ok(Self {
            doc,
            original: bytes,
            password: password.to_owned(),
            changed: BTreeSet::new(),
            next,
            crypt,
        })
    }

    /// Add an object; its number follows every number the file has used.
    /// When no number is left for it, it is not added, and saving refuses.
    pub fn add(&mut self, value: PdfObject) -> ObjectId {
        let number = u32::try_from(self.next).ok();
        self.next += 1;
        let Some(number) = number else {
            return ObjectId::new(0, 0);
        };
        let id = ObjectId::new(number, 0);
        self.set(id, value);
        id
    }

    /// Replace an object.
    pub fn set(&mut self, id: ObjectId, value: PdfObject) {
        self.doc.set_object(id, value);
        self.changed.insert(id);
    }

    /// A dictionary object, cloned for changing.
    pub fn dict(&self, id: ObjectId) -> Option<Dictionary> {
        self.doc.resolve(id).and_then(PdfObject::as_dict).cloned()
    }

    pub fn has_changes(&self) -> bool {
        !self.changed.is_empty()
    }

    /// The file with the edits: the original bytes and an update after
    /// them, or the whole file rewritten when its index was damaged.
    pub fn save(&self) -> Result<Vec<u8>> {
        if self.changed.is_empty() {
            return Ok(self.original.clone());
        }
        // The objects added have numbers up to 4294967295, the largest
        // there is, or some had none, and the file is left as it was.
        if self.next > 1 << 32 {
            return Err(PdfError::Structure(
                "the file's object numbers leave no room for the edits".into(),
            ));
        }
        let start = self.doc.xref.startxref;
        let appendable = self.doc.recovery_warnings.is_empty()
            && start > 0
            && start < self.original.len()
            && self.doc.root_ref().is_some();
        if !appendable {
            return self.rewrite();
        }
        let mut out = self.original.clone();
        if !out.ends_with(b"\n") && !out.ends_with(b"\r") {
            out.push(b'\n');
        }
        let mut offsets: Vec<(u32, u16, usize)> = Vec::new();
        for id in &self.changed {
            let Some(object) = self.doc.objects.get(id) else { continue };
            let mut value = object.value.clone();
            if let Some((crypt, encrypt_ref)) = &self.crypt {
                if *encrypt_ref != Some(*id) {
                    crypt.encrypt_object(*id, &mut value)?;
                }
            }
            offsets.push((id.number, id.generation, out.len()));
            writeln!(out, "{} {} obj", id.number, id.generation)?;
            out.extend_from_slice(&PdfWriter::serialize_object(&value)?);
            out.extend_from_slice(b"\nendobj\n");
        }
        let classic = self.original[start..]
            .iter()
            .position(|b| !b" \t\r\n\x0c\0".contains(b))
            .is_some_and(|skip| self.original[start + skip..].starts_with(b"xref"));
        let mut trailer = self.trailer();
        if classic {
            let size = self.next as i64;
            trailer.insert("Size".into(), PdfObject::Integer(size));
            let xref_at = out.len();
            out.extend_from_slice(b"xref\n");
            for run in runs(&offsets) {
                writeln!(out, "{} {}", run[0].0, run.len())?;
                for (_, generation, offset) in run {
                    write!(out, "{offset:010} {generation:05} n\r\n")?;
                }
            }
            out.extend_from_slice(b"trailer\n");
            out.extend_from_slice(&PdfWriter::serialize_object(&PdfObject::Dictionary(trailer))?);
            write!(out, "\nstartxref\n{xref_at}\n%%EOF\n")?;
        } else {
            // A cross-reference stream, as the file already uses them; it
            // lists itself.
            let own = u32::try_from(self.next).map_err(|_| {
                PdfError::Structure("the file's object numbers leave no room for the edits".into())
            })?;
            let size = u64::from(own) + 1;
            let xref_at = out.len();
            offsets.push((own, 0, xref_at));
            let mut data = Vec::new();
            let mut index = Vec::new();
            for run in runs(&offsets) {
                index.push(PdfObject::Integer(i64::from(run[0].0)));
                index.push(PdfObject::Integer(run.len() as i64));
                for (_, generation, offset) in run {
                    data.push(1);
                    data.extend_from_slice(&(*offset as u64).to_be_bytes()[3..]);
                    data.extend_from_slice(&generation.to_be_bytes());
                }
            }
            trailer.insert("Type".into(), PdfObject::Name("XRef".into()));
            trailer.insert("Size".into(), PdfObject::Integer(size as i64));
            trailer.insert("Index".into(), PdfObject::Array(index));
            trailer.insert(
                "W".into(),
                PdfObject::Array(vec![PdfObject::Integer(1), PdfObject::Integer(5), PdfObject::Integer(2)]),
            );
            writeln!(out, "{own} 0 obj")?;
            let stream = PdfObject::Stream(PdfStream::new(trailer, data));
            out.extend_from_slice(&PdfWriter::serialize_object(&stream)?);
            write!(out, "\nendobj\nstartxref\n{xref_at}\n%%EOF\n")?;
        }
        Ok(out)
    }

    /// The new section's trailer: the file's, pointing back at the
    /// section before it.
    fn trailer(&self) -> Dictionary {
        let mut trailer = Dictionary::new();
        for (key, value) in &self.doc.xref.trailer {
            if matches!(
                key.as_str(),
                "Root" | "Info" | "ID" | "Encrypt"
            ) {
                trailer.insert(key.clone(), value.clone());
            }
        }
        trailer.insert("Prev".into(), PdfObject::Integer(self.doc.xref.startxref as i64));
        trailer
    }

    /// The whole document written anew, for a file whose index could not
    /// be trusted. An encrypted one is encrypted again with its password.
    fn rewrite(&self) -> Result<Vec<u8>> {
        if self.doc.was_encrypted {
            return pdf_core::crypt::encrypt_to_bytes(&self.doc, &self.password, &self.password);
        }
        self.doc.to_bytes()
    }
}

/// Entries grouped into runs of consecutive object numbers.
fn runs(entries: &[(u32, u16, usize)]) -> Vec<&[(u32, u16, usize)]> {
    let mut out = Vec::new();
    let mut first = 0;
    while first < entries.len() {
        let mut end = first + 1;
        while end < entries.len() && entries[end].0 == entries[end - 1].0 + 1 {
            end += 1;
        }
        out.push(&entries[first..end]);
        first = end;
    }
    out
}
