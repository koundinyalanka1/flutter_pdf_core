//! Conservative reconstruction of a damaged cross-reference index.
//!
//! Only complete indirect objects are accepted. Parsing whole objects skips
//! their strings and binary streams, so object-like payload bytes do not become
//! document objects. Surviving xref free entries still prevent resurrection of
//! deleted objects. Encryption is authenticated by the normal document loader.

use std::collections::BTreeMap;

use crate::error::{PdfError, Result};
use crate::lexer::{is_ws, Lexer, Token};
use crate::object::{Dictionary, IndirectObject, ObjectId, PdfObject};
use crate::parser::Parser;
use crate::xref::{parse_xref_at, XrefEntry, XrefLocation, XrefTable};

pub(crate) fn reconstruct(
    data: &[u8],
    known: Option<XrefTable>,
) -> Result<(XrefTable, BTreeMap<ObjectId, IndirectObject>)> {
    let mut lexer = Lexer::new(data);
    let mut numbers = Vec::new();
    let mut scanned = BTreeMap::new();
    let mut trailer = Dictionary::new();
    let mut index = known;
    let mut encryption_dictionary = None;

    while lexer.position() < data.len() {
        let before = lexer.position();
        let token = match lexer.next_token() {
            Ok(Some(token)) => token,
            Ok(None) => break,
            Err(_) => {
                lexer.set_position(lexer.position().max(before + 1));
                numbers.clear();
                continue;
            }
        };
        if lexer.position() <= token.offset {
            // Unmatched delimiters may be returned as empty keywords. A
            // damaged byte must never leave the recovery scanner stationary.
            lexer.set_position(token.offset + 1);
            numbers.clear();
            continue;
        }
        match token.token {
            Token::Integer(_) => {
                numbers.push(token.offset);
                if numbers.len() > 2 {
                    numbers.remove(0);
                }
            }
            Token::Keyword(ref keyword) if keyword == "obj" && numbers.len() == 2 => {
                let offset = numbers[0];
                let mut parser = Parser::with_offset(data, offset);
                if let Ok(object) = parser.parse_indirect_object() {
                    if let PdfObject::Stream(stream) = &object.value {
                        if declared_stream_length(
                            data,
                            &stream.dictionary,
                            &scanned,
                            index.as_ref(),
                        )
                        .is_none_or(|length| length != stream.data.len())
                        {
                            // The parser's compatibility fallback scans for an
                            // endstream marker. A missing or damaged declared
                            // length makes that marker ambiguous during index
                            // reconstruction; payload bytes must not become
                            // apparent objects or trailers.
                            lexer.set_position(
                                failed_stream_end(data, lexer.position(), &scanned, index.as_ref())
                                    .unwrap_or(data.len()),
                            );
                            numbers.clear();
                            continue;
                        }
                    }
                    if let Some(dict) = object.value.as_dict() {
                        if dict.get("Filter").and_then(PdfObject::as_name) == Some("Standard")
                            && dict.contains_key("O")
                            && dict.contains_key("U")
                        {
                            encryption_dictionary = Some(object.id);
                        }
                    }
                    if let PdfObject::Stream(stream) = &object.value {
                        if stream.dictionary.get("Type").and_then(PdfObject::as_name)
                            == Some("XRef")
                        {
                            trailer.extend(stream.dictionary.clone());
                            if let Ok(found) = parse_xref_at(data, offset) {
                                retain_latest_index(&mut index, found);
                            }
                        }
                    }
                    // Newer complete definitions replace older revisions.
                    scanned.insert(object.id, (offset, object));
                    lexer.set_position(parser.position());
                } else if let Some(end) =
                    failed_stream_end(data, lexer.position(), &scanned, index.as_ref())
                {
                    // Never resume tokenization inside an incomplete binary
                    // stream: its bytes can look exactly like indirect objects.
                    lexer.set_position(end);
                }
                numbers.clear();
            }
            Token::Keyword(ref keyword) if keyword == "xref" => {
                if let Ok(found) = parse_xref_at(data, token.offset) {
                    retain_latest_index(&mut index, found);
                }
                numbers.clear();
            }
            Token::Keyword(ref keyword) if keyword == "trailer" => {
                let mut parser = Parser::with_offset(data, lexer.position());
                if let Ok(PdfObject::Dictionary(dict)) = parser.parse_object() {
                    trailer.extend(dict);
                    lexer.set_position(parser.position());
                }
                numbers.clear();
            }
            Token::Keyword(ref keyword) if keyword == "stream" => {
                // A malformed dictionary may prevent the more precise failed
                // stream boundary check above. There is no safe byte boundary.
                lexer.set_position(data.len());
                numbers.clear();
            }
            _ => numbers.clear(),
        }
    }
    let has_trailer = index.is_some() || !trailer.is_empty();
    let mut xref = index.unwrap_or(XrefTable {
        entries: BTreeMap::new(),
        trailer: Dictionary::new(),
        startxref: 0,
    });
    // Preserve encryption and file IDs from surviving trailers even if the
    // xref itself could not be parsed. Never treat encrypted bytes as plaintext.
    xref.trailer.extend(trailer);
    if !has_trailer && encryption_dictionary.is_some() {
        // An orphan encryption dictionary can also survive a decrypted rewrite.
        // Without a trailer, do not guess whether strings/streams are encrypted.
        return Err(PdfError::structure(
            "cannot recover missing encryption trailer",
        ));
    }
    let mut latest = BTreeMap::new();
    for (id, (offset, object)) in scanned {
        if id.number == 0 {
            continue;
        }
        if let Some(entry) = xref.entries.get(&id.number) {
            if offset <= xref.startxref
                && (!matches!(entry.location, XrefLocation::InFile { .. })
                    || entry.generation != id.generation)
            {
                continue;
            }
        }
        match latest.entry(id.number) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((offset, object));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if offset >= entry.get().0 {
                    entry.insert((offset, object));
                }
            }
        }
    }
    let mut objects = BTreeMap::new();
    for (number, (offset, object)) in latest {
        let id = object.id;
        xref.entries.insert(
            number,
            XrefEntry {
                location: XrefLocation::InFile { offset },
                generation: id.generation,
            },
        );
        objects.insert(id, object);
    }
    if objects.is_empty() {
        return Err(PdfError::structure(
            "no complete objects could be recovered",
        ));
    }
    Ok((xref, objects))
}

fn retain_latest_index(index: &mut Option<XrefTable>, found: XrefTable) {
    if index
        .as_ref()
        .is_none_or(|old| found.startxref >= old.startxref)
    {
        *index = Some(found);
    }
}

/// Once an indirect object has failed to parse, only its declared stream length
/// can establish a safe restart point. Searching binary data for `endstream`
/// would let payload bytes masquerade as complete document objects.
fn failed_stream_end(
    data: &[u8],
    body_start: usize,
    scanned: &BTreeMap<ObjectId, (usize, IndirectObject)>,
    index: Option<&XrefTable>,
) -> Option<usize> {
    let mut body = Parser::with_offset(data, body_start);
    let PdfObject::Dictionary(dictionary) = body.parse_object().ok()? else {
        return None;
    };
    let mut lexer = Lexer::with_offset(data, body.position());
    if !matches!(lexer.next_token().ok()??.token, Token::Keyword(word) if word == "stream") {
        return None;
    }
    let length = declared_stream_length(data, &dictionary, scanned, index);
    let Some(length) = length else {
        return Some(data.len());
    };
    let mut start = lexer.position();
    if data.get(start) == Some(&b'\r') {
        start += 1;
    }
    if data.get(start) == Some(&b'\n') {
        start += 1;
    }
    let Some(end) = start.checked_add(length).filter(|&end| end <= data.len()) else {
        return Some(data.len());
    };
    let mut marker = end;
    while marker < data.len() && marker - end < 4 && is_ws(data[marker]) {
        marker += 1;
    }
    if data[marker..].starts_with(b"endstream") {
        Some(marker + b"endstream".len())
    } else {
        Some(data.len())
    }
}

fn declared_stream_length(
    data: &[u8],
    dictionary: &Dictionary,
    scanned: &BTreeMap<ObjectId, (usize, IndirectObject)>,
    index: Option<&XrefTable>,
) -> Option<usize> {
    dictionary.get("Length").and_then(|length| match length {
        PdfObject::Integer(value) => usize::try_from(*value).ok(),
        PdfObject::Reference(id) => {
            let value = scanned
                .get(id)
                .map(|(_, object)| object.value.clone())
                .or_else(|| {
                    let entry = index?.get(*id)?;
                    let XrefLocation::InFile { offset } = entry.location else {
                        return None;
                    };
                    let object = Parser::with_offset(data, offset)
                        .parse_indirect_object()
                        .ok()?;
                    (object.id == *id).then_some(object.value)
                })?;
            usize::try_from(value.as_i64()?).ok()
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::PdfDocument;
    use crate::stream::PdfStream;
    use crate::xref::parse_xref;

    fn broken_startxref(bytes: &[u8]) -> Vec<u8> {
        let at = bytes.windows(9).rposition(|w| w == b"startxref").unwrap();
        let mut result = bytes[..at].to_vec();
        result.extend_from_slice(b"startxref\n999999999\n%%EOF\n");
        result
    }

    fn fixture() -> PdfDocument {
        PdfDocument::from_bytes(include_bytes!("../../../fixtures/simple.pdf")).unwrap()
    }

    #[test]
    fn repairs_one_bad_object_offset_without_losing_other_objects() {
        let original = fixture();
        let mut bytes = original.to_bytes().unwrap();
        let table = parse_xref(&bytes).unwrap();
        let old = format!("{:010}", table.entries[&1].offset().unwrap());
        let at = bytes
            .windows(old.len())
            .rposition(|w| w == old.as_bytes())
            .unwrap();
        bytes[at..at + 10].copy_from_slice(b"9999999999");
        let recovered = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(recovered.page_count(), original.page_count());
        assert_eq!(recovered.objects, original.objects);
        assert_eq!(recovered.recovery_warnings.len(), 1);
    }

    #[test]
    fn recovers_wrong_startxref_and_truncated_index_without_modifying_input() {
        let bytes = fixture().to_bytes().unwrap();
        let index = parse_xref(&bytes).unwrap().startxref;
        for damaged in [
            broken_startxref(&bytes),
            bytes[..index].to_vec(),
            bytes[..index + 15].to_vec(),
        ] {
            let copy = damaged.clone();
            let recovered = PdfDocument::from_bytes(&damaged).unwrap();
            assert_eq!(recovered.page_count(), Some(1));
            assert!(!recovered.recovery_warnings.is_empty());
            assert_eq!(damaged, copy);
            let saved = PdfDocument::from_bytes(&recovered.to_bytes().unwrap()).unwrap();
            assert_eq!(saved.page_count(), Some(1));
            assert!(saved.recovery_warnings.is_empty());
        }
    }

    #[test]
    fn ignores_fake_object_headers_inside_streams_and_strings() {
        let mut doc = fixture();
        doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::new(),
            b"\n999 0 obj << /Type /Catalog >> endobj\n".to_vec(),
        )));
        doc.add_object(PdfObject::LiteralString(b"998 0 obj null endobj".to_vec()));
        let recovered =
            PdfDocument::from_bytes(&broken_startxref(&doc.to_bytes().unwrap())).unwrap();
        assert!(recovered.resolve(ObjectId::new(999, 0)).is_none());
        assert!(recovered.resolve(ObjectId::new(998, 0)).is_none());
        assert_eq!(recovered.page_count(), Some(1));
    }

    #[test]
    fn never_recovers_object_headers_from_an_incomplete_stream_payload() {
        let bytes = fixture().to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        for payload in [
            "999 0 obj << /Type /Catalog >> endobj\n",
            "endstream\nendobj\n999 0 obj << /Type /Catalog >> endobj\n",
        ] {
            let mut damaged = bytes[..end].to_vec();
            damaged.extend_from_slice(b"77 0 obj << /Length 9999999 >>\nstream\n");
            damaged.extend_from_slice(payload.as_bytes());
            let recovered = PdfDocument::from_bytes(&damaged).unwrap();
            assert_eq!(recovered.page_count(), Some(1));
            assert!(recovered.resolve(ObjectId::new(77, 0)).is_none());
            assert!(recovered.resolve(ObjectId::new(999, 0)).is_none());
        }
    }

    #[test]
    fn unresolved_stream_lengths_do_not_trust_markers_inside_payload_bytes() {
        let bytes = fixture().to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        for dictionary in ["<< >>", "<< /Length -1 >>", "<< /Length 78 0 R >>"] {
            let mut damaged = bytes[..end].to_vec();
            damaged.extend_from_slice(format!("77 0 obj {dictionary}\nstream\n").as_bytes());
            // This marker could be literal binary data. Without a verifiable
            // length, none of the apparent definitions after it is trustworthy.
            damaged
                .extend_from_slice(b"endstream\nendobj\n999 0 obj << /Type /Catalog >> endobj\n");
            let recovered = PdfDocument::from_bytes(&damaged).unwrap();
            assert_eq!(recovered.page_count(), Some(1));
            assert!(recovered.resolve(ObjectId::new(77, 0)).is_none());
            assert!(recovered.resolve(ObjectId::new(999, 0)).is_none());
        }
    }

    #[test]
    fn resumes_after_a_known_stream_boundary_even_when_endobj_is_missing() {
        let bytes = fixture().to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        let payload = b"999 0 obj null endobj\n";
        let mut damaged = bytes[..end].to_vec();
        damaged.extend_from_slice(
            format!("77 0 obj << /Length {} >>\nstream\n", payload.len()).as_bytes(),
        );
        damaged.extend_from_slice(payload);
        damaged.extend_from_slice(b"\nendstream\n78 0 obj (survives) endobj\n");
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        // Nothing inside the stream's bytes is mistaken for an object.
        assert!(recovered.resolve(ObjectId::new(999, 0)).is_none());
        // The stream ends exactly at its /Length and the next object follows,
        // so it is complete even without `endobj`, as some writers emit it.
        match recovered.resolve(ObjectId::new(77, 0)) {
            Some(PdfObject::Stream(stream)) => assert_eq!(stream.data, payload),
            other => panic!("expected the complete stream, got {other:?}"),
        }
        assert_eq!(
            recovered.resolve(ObjectId::new(78, 0)),
            Some(&PdfObject::LiteralString(b"survives".to_vec()))
        );
    }

    #[test]
    fn newer_complete_reused_objects_override_an_older_free_xref_entry() {
        let mut doc = fixture();
        let old_id = doc.add_object(PdfObject::Null);
        let bytes = doc.to_bytes().unwrap();
        let table = parse_xref(&bytes).unwrap();
        let line = format!(
            "{:010} 00000 n",
            table.entries[&old_id.number].offset().unwrap()
        );
        let mut damaged = bytes.clone();
        let at = damaged
            .windows(line.len())
            .position(|w| w == line.as_bytes())
            .unwrap();
        damaged[at..at + line.len()].copy_from_slice(b"0000000000 00001 f");
        damaged.extend_from_slice(
            format!(
                "\n{} 1 obj (new revision) endobj\nstartxref\n999999999\n%%EOF\n",
                old_id.number
            )
            .as_bytes(),
        );
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert!(recovered.resolve(old_id).is_none());
        assert_eq!(
            recovered.resolve(ObjectId::new(old_id.number, 1)),
            Some(&PdfObject::LiteralString(b"new revision".to_vec()))
        );
    }

    fn append_object_stream(bytes: &mut Vec<u8>, container: u32, number: u32, body: &str) {
        let header = format!("{number} 0 ");
        let payload = format!("{header}{body}");
        bytes.extend_from_slice(format!("\n{container} 0 obj << /Type /ObjStm /N 1 /First {} /Length {} >>\nstream\n{payload}\nendstream\nendobj\n", header.len(), payload.len()).as_bytes());
    }

    #[test]
    fn compressed_revisions_use_file_order_and_supersede_older_plain_objects() {
        let bytes = fixture().to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        for has_index in [false, true] {
            let mut damaged = if has_index {
                bytes.clone()
            } else {
                bytes[..end].to_vec()
            };
            damaged.extend_from_slice(b"\n77 0 obj (old plain) endobj\n");
            append_object_stream(&mut damaged, 100, 77, "(old compressed)");
            append_object_stream(&mut damaged, 90, 77, "(new compressed)");
            damaged.extend_from_slice(b"startxref\n999999999\n%%EOF\n");
            let recovered = PdfDocument::from_bytes(&damaged).unwrap();
            assert_eq!(
                recovered.resolve(ObjectId::new(77, 0)),
                Some(&PdfObject::LiteralString(b"new compressed".to_vec()))
            );
            damaged.extend_from_slice(
                b"77 0 obj (newest plain) endobj\nstartxref\n999999999\n%%EOF\n",
            );
            let recovered = PdfDocument::from_bytes(&damaged).unwrap();
            assert_eq!(
                recovered.resolve(ObjectId::new(77, 0)),
                Some(&PdfObject::LiteralString(b"newest plain".to_vec()))
            );
        }
    }

    #[test]
    fn missing_root_uses_latest_physical_catalog_including_compressed_revisions() {
        let doc = fixture();
        let pages = doc
            .resolve(doc.root_ref().unwrap())
            .unwrap()
            .as_dict()
            .unwrap()["Pages"]
            .as_ref()
            .unwrap();
        let body = format!(
            "<< /Type /Catalog /Pages {} {} R >>",
            pages.number, pages.generation
        );
        let bytes = doc.to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        let mut damaged = bytes[..end].to_vec();
        damaged.extend_from_slice(format!("77 0 obj {body} endobj\n").as_bytes());
        append_object_stream(&mut damaged, 88, 55, &body);
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert_eq!(recovered.root_ref(), Some(ObjectId::new(55, 0)));
        assert_eq!(recovered.page_count(), Some(1));

        damaged.extend_from_slice(format!("44 0 obj {body} endobj\n").as_bytes());
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert_eq!(recovered.root_ref(), Some(ObjectId::new(44, 0)));
        assert_eq!(recovered.page_count(), Some(1));
    }

    #[test]
    fn compressed_objects_cannot_resurrect_a_newer_free_index_entry() {
        let bytes = fixture().to_bytes().unwrap();
        let previous_xref = parse_xref(&bytes).unwrap().startxref;
        let mut damaged = bytes;
        append_object_stream(&mut damaged, 88, 77, "(deleted compressed object)");
        damaged.extend_from_slice(
            format!(
                "xref\n77 1\n0000000000 00001 f \ntrailer\n<< /Size 89 /Prev {previous_xref} >>\nstartxref\n999999999\n%%EOF\n"
            )
            .as_bytes(),
        );
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert_eq!(recovered.page_count(), Some(1));
        assert!(recovered.resolve(ObjectId::new(77, 0)).is_none());
    }

    #[test]
    fn partial_incremental_trailers_do_not_drop_encryption_authentication() {
        let encrypted = crate::crypt::encrypt_to_bytes(&fixture(), "user-pw", "owner-pw").unwrap();
        for tail in [
            b"\ntrailer\n<< /Size 80 >>\nstartxref\n999999999\n%%EOF\n".as_slice(),
            b"\ntrailer\n<< /Size 80 /Encrypt".as_slice(),
        ] {
            let mut damaged = broken_startxref(&encrypted);
            damaged.extend_from_slice(tail);
            assert!(matches!(
                PdfDocument::from_bytes(&damaged),
                Err(PdfError::Encrypted)
            ));
            assert!(matches!(
                PdfDocument::from_bytes_with_password(&damaged, "wrong"),
                Err(PdfError::WrongPassword)
            ));
            for password in ["user-pw", "owner-pw"] {
                let recovered = PdfDocument::from_bytes_with_password(&damaged, password).unwrap();
                assert!(recovered.was_encrypted);
                assert_eq!(recovered.page_count(), Some(1));
            }
        }
    }

    #[test]
    fn respects_deleted_objects_in_surviving_cross_reference() {
        let mut doc = fixture();
        let orphan = doc.add_object(PdfObject::LiteralString(b"deleted".to_vec()));
        let bytes = doc.to_bytes().unwrap();
        let table = parse_xref(&bytes).unwrap();
        let line = format!(
            "{:010} 00000 n",
            table.entries[&orphan.number].offset().unwrap()
        );
        let mut damaged = broken_startxref(&bytes);
        let at = damaged
            .windows(line.len())
            .position(|w| w == line.as_bytes())
            .unwrap();
        damaged[at + line.len() - 1] = b'f';
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert!(recovered.resolve(orphan).is_none());
    }

    #[test]
    fn recovered_encrypted_files_still_require_the_correct_password() {
        let encrypted = crate::crypt::encrypt_to_bytes(&fixture(), "user-pw", "owner-pw").unwrap();
        let damaged = broken_startxref(&encrypted);
        assert!(matches!(
            PdfDocument::from_bytes(&damaged),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            PdfDocument::from_bytes_with_password(&damaged, "wrong"),
            Err(PdfError::WrongPassword)
        ));
        for password in ["user-pw", "owner-pw"] {
            let doc = PdfDocument::from_bytes_with_password(&damaged, password).unwrap();
            assert!(doc.was_encrypted);
            assert_eq!(doc.page_count(), Some(1));
        }
    }

    #[test]
    fn orphan_encryption_dictionary_does_not_reencrypt_a_plain_rewrite() {
        let encrypted = crate::crypt::encrypt_to_bytes(&fixture(), "password", "").unwrap();
        let opened = PdfDocument::from_bytes_with_password(&encrypted, "password").unwrap();
        let plain = opened.to_bytes().unwrap();
        let recovered = PdfDocument::from_bytes(&broken_startxref(&plain)).unwrap();
        assert!(!recovered.was_encrypted);
        assert_eq!(recovered.page_count(), Some(1));
    }

    #[test]
    fn does_not_fabricate_a_document_from_incomplete_objects() {
        assert!(PdfDocument::from_bytes(b"%PDF-1.7\n1 0 obj << /Type /Catalog").is_err());
        assert!(PdfDocument::from_bytes(b"%PDF-1.7\n1 0 obj null endobj").is_err());
    }

    #[test]
    fn skips_unmatched_delimiters_without_stalling_the_recovery_scanner() {
        let bytes = fixture().to_bytes().unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        let mut damaged = bytes[..end].to_vec();
        damaged.extend_from_slice(b") > ) 77 0 obj (survives) endobj\n");
        let recovered = PdfDocument::from_bytes(&damaged).unwrap();
        assert_eq!(recovered.page_count(), Some(1));
        assert_eq!(
            recovered.resolve(ObjectId::new(77, 0)),
            Some(&PdfObject::LiteralString(b"survives".to_vec()))
        );
    }

    #[test]
    fn newest_complete_generation_wins_without_an_index() {
        let bytes = fixture().to_bytes().unwrap();
        let at = parse_xref(&bytes).unwrap().startxref;
        let mut damaged = bytes[..at].to_vec();
        damaged.extend_from_slice(b"77 0 obj (old) endobj\n77 1 obj (new) endobj\n");
        let doc = PdfDocument::from_bytes(&damaged).unwrap();
        assert!(doc.resolve(ObjectId::new(77, 0)).is_none());
        assert_eq!(
            doc.resolve(ObjectId::new(77, 1)),
            Some(&PdfObject::LiteralString(b"new".to_vec()))
        );
    }

    #[test]
    fn recovers_page_tree_from_object_stream_without_any_cross_reference() {
        let bodies = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ];
        let mut header = String::new();
        let mut body = String::new();
        for (i, object) in bodies.iter().enumerate() {
            header.push_str(&format!("{} {} ", i + 1, body.len()));
            body.push_str(object);
            body.push(' ');
        }
        let payload = crate::filter::flate_encode(format!("{header}{body}").as_bytes());
        let mut bytes = format!("%PDF-1.7\n8 0 obj << /Type /ObjStm /N 3 /First {} /Filter /FlateDecode /Length {} >>\nstream\n", header.len(), payload.len()).into_bytes();
        bytes.extend(payload);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        let recovered = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(recovered.page_count(), Some(1));
        assert_eq!(recovered.root_ref(), Some(ObjectId::new(1, 0)));
        assert!(!recovered.recovery_warnings.is_empty());
    }

    #[test]
    fn refuses_ambiguous_encryption_when_the_entire_trailer_is_lost() {
        let bytes = crate::crypt::encrypt_to_bytes(&fixture(), "password", "").unwrap();
        let end = parse_xref(&bytes).unwrap().startxref;
        assert!(PdfDocument::from_bytes_with_password(&bytes[..end], "password").is_err());
    }
}
