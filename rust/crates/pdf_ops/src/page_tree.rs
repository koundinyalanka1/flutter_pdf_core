//! Milestone 3: a proper page tree model.
//!
//! Walks arbitrarily nested `/Pages` trees, resolves the inheritable page
//! attributes (`Resources`, `MediaBox`, `CropBox`, `Rotate`) and can rebuild
//! a clean, flat tree after pages have been added, removed or reordered.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};

pub const INHERITABLE_KEYS: [&str; 4] = ["Resources", "MediaBox", "CropBox", "Rotate"];

/// A page with its inherited attributes materialized.
#[derive(Debug, Clone)]
pub struct PageRef {
    pub id: ObjectId,
    /// The page's own dictionary plus any attributes inherited from
    /// ancestor `/Pages` nodes.
    pub effective: Dictionary,
}

/// Ordered list of pages with inheritance resolved.
pub fn pages(doc: &PdfDocument) -> Result<Vec<PageRef>> {
    let ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    ids.into_iter()
        .map(|id| {
            let effective = effective_page_dict(doc, id)?;
            Ok(PageRef { id, effective })
        })
        .collect()
}

pub fn page_count(doc: &PdfDocument) -> Result<usize> {
    Ok(doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?
        .len())
}

/// The page dictionary with inherited attributes copied in.
pub fn effective_page_dict(doc: &PdfDocument, page_id: ObjectId) -> Result<Dictionary> {
    let mut dict = doc
        .resolve(page_id)
        .and_then(PdfObject::as_dict)
        .cloned()
        .ok_or_else(|| PdfError::Structure(format!("page object {page_id:?} missing")))?;
    // Walk up /Parent links for any missing inheritable attribute.
    let mut parent = dict.get("Parent").and_then(PdfObject::as_ref);
    let mut depth = 0;
    while let Some(parent_id) = parent {
        depth += 1;
        if depth > 256 {
            break;
        }
        let Some(parent_dict) = doc.resolve(parent_id).and_then(PdfObject::as_dict) else {
            break;
        };
        for key in INHERITABLE_KEYS {
            if !dict.contains_key(key) {
                if let Some(value) = parent_dict.get(key) {
                    dict.insert(key.to_owned(), value.clone());
                }
            }
        }
        parent = parent_dict.get("Parent").and_then(PdfObject::as_ref);
    }
    Ok(dict)
}

/// Resolve a single attribute for a page, honoring inheritance.
pub fn page_attribute(doc: &PdfDocument, page_id: ObjectId, key: &str) -> Option<PdfObject> {
    let mut current = Some(page_id);
    let mut depth = 0;
    while let Some(id) = current {
        depth += 1;
        if depth > 256 {
            return None;
        }
        let dict = doc.resolve(id).and_then(PdfObject::as_dict)?;
        if let Some(value) = dict.get(key) {
            return Some(doc.resolve_value(value));
        }
        current = dict.get("Parent").and_then(PdfObject::as_ref);
    }
    None
}

/// Rebuild the document's page tree as a single flat `/Pages` node holding
/// `ordered_pages` (which must already exist as objects in the document).
///
/// Each page gets its inherited attributes materialized so nothing is lost
/// when its old ancestors disappear, then `/Parent` is repointed at the new
/// root node. The catalog's `/Pages` entry is updated in place.
pub fn rebuild_page_tree(doc: &mut PdfDocument, ordered_pages: &[ObjectId]) -> Result<()> {
    let root_id = doc
        .root_ref()
        .ok_or_else(|| PdfError::Structure("missing /Root".into()))?;

    // Materialize inheritance before tearing down the old tree.
    let mut effective: Vec<(ObjectId, Dictionary)> = Vec::with_capacity(ordered_pages.len());
    for &page_id in ordered_pages {
        effective.push((page_id, effective_page_dict(doc, page_id)?));
    }

    // Reuse the existing /Pages object id when possible so the catalog's
    // reference stays valid in incremental tooling; otherwise allocate.
    let existing_pages_id = doc
        .catalog()
        .and_then(|cat| cat.get("Pages"))
        .and_then(PdfObject::as_ref);
    let pages_id = existing_pages_id.unwrap_or_else(|| doc.add_object(PdfObject::Null));

    for (page_id, mut dict) in effective {
        dict.insert("Type".into(), PdfObject::Name("Page".into()));
        dict.insert("Parent".into(), PdfObject::Reference(pages_id));
        doc.set_object(page_id, PdfObject::Dictionary(dict));
    }

    let mut pages_dict = Dictionary::new();
    pages_dict.insert("Type".into(), PdfObject::Name("Pages".into()));
    pages_dict.insert(
        "Kids".into(),
        PdfObject::Array(
            ordered_pages
                .iter()
                .map(|id| PdfObject::Reference(*id))
                .collect(),
        ),
    );
    pages_dict.insert(
        "Count".into(),
        PdfObject::Integer(ordered_pages.len() as i64),
    );
    doc.set_object(pages_id, PdfObject::Dictionary(pages_dict));

    // Point the catalog at the rebuilt tree.
    let mut catalog = doc
        .resolve(root_id)
        .and_then(PdfObject::as_dict)
        .cloned()
        .ok_or_else(|| PdfError::Structure("missing catalog".into()))?;
    catalog.insert("Pages".into(), PdfObject::Reference(pages_id));
    doc.set_object(root_id, PdfObject::Dictionary(catalog));
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Build an in-memory document with `n` pages under a nested tree:
    /// Pages(root) -> [Pages(inner) -> [page0, page1, ...]] with MediaBox
    /// and Rotate set on ancestors to exercise inheritance.
    pub fn nested_doc(n: usize) -> PdfDocument {
        let mut doc = PdfDocument::new_empty("1.7");
        let root_pages = ObjectId::new(2, 0);
        let inner_pages = ObjectId::new(3, 0);

        let mut page_ids = Vec::new();
        for i in 0..n {
            let mut page = Dictionary::new();
            page.insert("Type".into(), PdfObject::Name("Page".into()));
            page.insert("Parent".into(), PdfObject::Reference(inner_pages));
            // Give each page a marker content-free attribute.
            page.insert("PageIndexMarker".into(), PdfObject::Integer(i as i64));
            let id = ObjectId::new(10 + i as u32, 0);
            doc.set_object(id, PdfObject::Dictionary(page));
            page_ids.push(id);
        }

        let mut inner = Dictionary::new();
        inner.insert("Type".into(), PdfObject::Name("Pages".into()));
        inner.insert(
            "Kids".into(),
            PdfObject::Array(page_ids.iter().map(|id| PdfObject::Reference(*id)).collect()),
        );
        inner.insert("Count".into(), PdfObject::Integer(n as i64));
        inner.insert("Parent".into(), PdfObject::Reference(root_pages));
        inner.insert("Rotate".into(), PdfObject::Integer(90));
        doc.set_object(inner_pages, PdfObject::Dictionary(inner));

        let mut root = Dictionary::new();
        root.insert("Type".into(), PdfObject::Name("Pages".into()));
        root.insert(
            "Kids".into(),
            PdfObject::Array(vec![PdfObject::Reference(inner_pages)]),
        );
        root.insert("Count".into(), PdfObject::Integer(n as i64));
        root.insert(
            "MediaBox".into(),
            PdfObject::Array(vec![
                PdfObject::Integer(0),
                PdfObject::Integer(0),
                PdfObject::Integer(612),
                PdfObject::Integer(792),
            ]),
        );
        doc.set_object(root_pages, PdfObject::Dictionary(root));

        let mut catalog = Dictionary::new();
        catalog.insert("Type".into(), PdfObject::Name("Catalog".into()));
        catalog.insert("Pages".into(), PdfObject::Reference(root_pages));
        let catalog_id = ObjectId::new(1, 0);
        doc.set_object(catalog_id, PdfObject::Dictionary(catalog));
        doc.set_trailer_key("Root", PdfObject::Reference(catalog_id));
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::nested_doc;
    use super::*;

    #[test]
    fn walks_nested_tree_in_order() {
        let doc = nested_doc(3);
        let pages = pages(&doc).unwrap();
        assert_eq!(pages.len(), 3);
        for (i, page) in pages.iter().enumerate() {
            assert_eq!(
                page.effective["PageIndexMarker"].as_i64(),
                Some(i as i64),
                "pages should come back in document order"
            );
        }
    }

    #[test]
    fn inherits_attributes_from_ancestors() {
        let doc = nested_doc(2);
        let pages = pages(&doc).unwrap();
        // Rotate comes from the inner node, MediaBox from the root node.
        assert_eq!(pages[0].effective["Rotate"].as_i64(), Some(90));
        assert!(matches!(
            pages[0].effective.get("MediaBox"),
            Some(PdfObject::Array(_))
        ));
        assert_eq!(
            page_attribute(&doc, pages[0].id, "Rotate").and_then(|o| o.as_i64()),
            Some(90)
        );
    }

    #[test]
    fn rebuild_produces_flat_tree_and_keeps_inheritance() {
        let mut doc = nested_doc(3);
        let ids = doc.collect_page_ids().unwrap();
        // Keep only the last page, reversed order scenario.
        rebuild_page_tree(&mut doc, &[ids[2], ids[0]]).unwrap();

        let pages = pages(&doc).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].effective["PageIndexMarker"].as_i64(), Some(2));
        assert_eq!(pages[1].effective["PageIndexMarker"].as_i64(), Some(0));
        // Inherited values were materialized onto the page itself.
        assert_eq!(pages[0].effective["Rotate"].as_i64(), Some(90));
        assert!(pages[0].effective.contains_key("MediaBox"));
        // Round-trips through the writer.
        let bytes = doc.to_bytes().unwrap();
        let reread = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(reread.page_count(), Some(2));
    }
}
