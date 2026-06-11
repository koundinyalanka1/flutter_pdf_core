//! Milestone 4: split, delete and reorder pages.
//!
//! Extraction copies the transitive closure of every kept page (contents,
//! resources, annotations, …) into a fresh document with compact object
//! numbering, so split output contains no dead weight from dropped pages.

use std::collections::{BTreeMap, VecDeque};

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};

use crate::page_tree::{effective_page_dict, rebuild_page_tree};

/// Copy `roots` (and everything they reference) from `source` into `target`.
/// Returns the new ids assigned to each root, in order.
pub(crate) fn copy_objects_into(
    target: &mut PdfDocument,
    source: &PdfDocument,
    roots: &[(ObjectId, PdfObject)],
) -> Result<Vec<ObjectId>> {
    let mut map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    let mut pending: Vec<(ObjectId, PdfObject)> = Vec::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    let mut root_ids = Vec::with_capacity(roots.len());

    // Seed with the (possibly rewritten) root values.
    for (old_id, value) in roots {
        let new_id = target.add_object(PdfObject::Null);
        map.insert(*old_id, new_id);
        root_ids.push(new_id);
        collect_refs(value, &mut queue);
        pending.push((new_id, value.clone()));
    }

    // Transitive closure over the source document.
    while let Some(old_id) = queue.pop_front() {
        if map.contains_key(&old_id) {
            continue;
        }
        let Some(object) = source.objects.get(&old_id) else {
            continue; // dangling reference: will be rewritten to null
        };
        let new_id = target.add_object(PdfObject::Null);
        map.insert(old_id, new_id);
        collect_refs(&object.value, &mut queue);
        pending.push((new_id, object.value.clone()));
    }

    // Rewrite references and install the objects.
    for (new_id, mut value) in pending {
        rewrite_refs(&mut value, &map);
        target.set_object(new_id, value);
    }
    Ok(root_ids)
}

fn collect_refs(object: &PdfObject, queue: &mut VecDeque<ObjectId>) {
    match object {
        PdfObject::Reference(id) => queue.push_back(*id),
        PdfObject::Array(items) => items.iter().for_each(|i| collect_refs(i, queue)),
        PdfObject::Dictionary(dict) => dict.values().for_each(|v| collect_refs(v, queue)),
        PdfObject::Stream(stream) => stream
            .dictionary
            .values()
            .for_each(|v| collect_refs(v, queue)),
        _ => {}
    }
}

fn rewrite_refs(object: &mut PdfObject, map: &BTreeMap<ObjectId, ObjectId>) {
    match object {
        PdfObject::Reference(id) => match map.get(id) {
            Some(new_id) => *id = *new_id,
            None => *object = PdfObject::Null,
        },
        PdfObject::Array(items) => items.iter_mut().for_each(|i| rewrite_refs(i, map)),
        PdfObject::Dictionary(dict) => dict.values_mut().for_each(|v| rewrite_refs(v, map)),
        PdfObject::Stream(stream) => stream
            .dictionary
            .values_mut()
            .for_each(|v| rewrite_refs(v, map)),
        _ => {}
    }
}

/// Copy the given pages of `source` (0-based indices) into a brand-new
/// document, in the order given. Indices may repeat (page duplication).
pub fn extract_pages(source: &PdfDocument, indices: &[usize]) -> Result<PdfDocument> {
    let page_ids = source
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    let mut roots = Vec::with_capacity(indices.len());
    for &index in indices {
        let &page_id = page_ids.get(index).ok_or(PdfError::PageIndex(index))?;
        let mut dict = effective_page_dict(source, page_id)?;
        dict.remove("Parent"); // re-parented after the copy
        roots.push((page_id, PdfObject::Dictionary(dict)));
    }

    let mut target = PdfDocument::new_empty(&source.version);
    // Each requested index gets its own copy pass so a repeated index
    // yields a distinct page object (viewer-friendly duplication).
    let mut new_page_ids = Vec::with_capacity(indices.len());
    for root in roots {
        let ids = copy_objects_into(&mut target, source, &[root])?;
        new_page_ids.extend(ids);
    }

    install_catalog(&mut target, &new_page_ids)?;

    // Carry document metadata along.
    if let Some(info_id) = source.info_ref() {
        if let Some(info) = source.resolve(info_id) {
            let ids = copy_objects_into(&mut target, source, &[(info_id, info.clone())])?;
            if let Some(&new_info) = ids.first() {
                target.set_trailer_key("Info", PdfObject::Reference(new_info));
            }
        }
    }
    Ok(target)
}

/// Create catalog + flat page tree for a fresh document.
pub(crate) fn install_catalog(target: &mut PdfDocument, page_ids: &[ObjectId]) -> Result<()> {
    let pages_id = target.add_object(PdfObject::Null);
    let mut catalog = Dictionary::new();
    catalog.insert("Type".into(), PdfObject::Name("Catalog".into()));
    catalog.insert("Pages".into(), PdfObject::Reference(pages_id));
    let catalog_id = target.add_object(PdfObject::Dictionary(catalog));
    target.set_trailer_key("Root", PdfObject::Reference(catalog_id));
    rebuild_page_tree(target, page_ids)
}

/// Split into one single-page document per page.
pub fn split_into_pages(source: &PdfDocument) -> Result<Vec<PdfDocument>> {
    let n = source
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?
        .len();
    (0..n).map(|i| extract_pages(source, &[i])).collect()
}

/// Remove the given pages (0-based, in any order) in place.
pub fn delete_pages(doc: &mut PdfDocument, indices: &[usize]) -> Result<()> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    for &index in indices {
        if index >= page_ids.len() {
            return Err(PdfError::PageIndex(index));
        }
    }
    let keep: Vec<ObjectId> = page_ids
        .iter()
        .enumerate()
        .filter(|(i, _)| !indices.contains(i))
        .map(|(_, id)| *id)
        .collect();
    if keep.is_empty() {
        return Err(PdfError::Structure(
            "cannot delete every page of a document".into(),
        ));
    }
    rebuild_page_tree(doc, &keep)?;
    doc.garbage_collect();
    Ok(())
}

/// Reorder pages in place. `order` must be a permutation of `0..page_count`.
pub fn reorder_pages(doc: &mut PdfDocument, order: &[usize]) -> Result<()> {
    let page_ids = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?;
    if order.len() != page_ids.len() {
        return Err(PdfError::Structure(format!(
            "order has {} entries but the document has {} pages",
            order.len(),
            page_ids.len()
        )));
    }
    let mut seen = vec![false; page_ids.len()];
    for &index in order {
        if index >= page_ids.len() || seen[index] {
            return Err(PdfError::Structure(
                "order must be a permutation of all page indices".into(),
            ));
        }
        seen[index] = true;
    }
    let reordered: Vec<ObjectId> = order.iter().map(|&i| page_ids[i]).collect();
    rebuild_page_tree(doc, &reordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_tree::pages;
    use crate::page_tree::test_support::nested_doc;

    #[test]
    fn extracts_subset_with_inherited_attributes() {
        let doc = nested_doc(4);
        let out = extract_pages(&doc, &[3, 1]).unwrap();
        let pages = pages(&out).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].effective["PageIndexMarker"].as_i64(), Some(3));
        assert_eq!(pages[1].effective["PageIndexMarker"].as_i64(), Some(1));
        // Inheritance materialized from old ancestors.
        assert_eq!(pages[0].effective["Rotate"].as_i64(), Some(90));
        assert!(pages[0].effective.contains_key("MediaBox"));
        // Writes and reparses cleanly.
        let bytes = out.to_bytes().unwrap();
        assert_eq!(
            PdfDocument::from_bytes(&bytes).unwrap().page_count(),
            Some(2)
        );
    }

    #[test]
    fn extract_rejects_out_of_bounds() {
        let doc = nested_doc(2);
        assert!(matches!(
            extract_pages(&doc, &[5]),
            Err(PdfError::PageIndex(5))
        ));
    }

    #[test]
    fn split_into_single_pages() {
        let doc = nested_doc(3);
        let parts = split_into_pages(&doc).unwrap();
        assert_eq!(parts.len(), 3);
        for (i, part) in parts.iter().enumerate() {
            let pages = pages(part).unwrap();
            assert_eq!(pages.len(), 1);
            assert_eq!(
                pages[0].effective["PageIndexMarker"].as_i64(),
                Some(i as i64)
            );
        }
    }

    #[test]
    fn deletes_pages_and_garbage_collects() {
        let mut doc = nested_doc(3);
        let before = doc.objects.len();
        delete_pages(&mut doc, &[1]).unwrap();
        let pages = pages(&doc).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].effective["PageIndexMarker"].as_i64(), Some(0));
        assert_eq!(pages[1].effective["PageIndexMarker"].as_i64(), Some(2));
        assert!(doc.objects.len() < before, "deleted page should be GC'd");
        assert!(delete_pages(&mut doc, &[0, 1]).is_err(), "cannot empty doc");
    }

    #[test]
    fn reorders_pages() {
        let mut doc = nested_doc(3);
        reorder_pages(&mut doc, &[2, 0, 1]).unwrap();
        let pages = pages(&doc).unwrap();
        let markers: Vec<i64> = pages
            .iter()
            .map(|p| p.effective["PageIndexMarker"].as_i64().unwrap())
            .collect();
        assert_eq!(markers, vec![2, 0, 1]);
        assert!(reorder_pages(&mut doc, &[0, 0, 1]).is_err());
        assert!(reorder_pages(&mut doc, &[0]).is_err());
    }
}
