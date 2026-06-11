//! Milestone 5: merge PDFs.
//!
//! Every source document's pages are copied — with object renumbering and
//! reference rewriting — into one fresh document with a flat page tree.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{ObjectId, PdfObject};

use crate::page_tree::effective_page_dict;
use crate::split::{copy_objects_into, install_catalog};

/// Merge whole documents, in order.
pub fn merge_documents(sources: &[&PdfDocument]) -> Result<PdfDocument> {
    if sources.is_empty() {
        return Err(PdfError::Structure("nothing to merge".into()));
    }
    // Use the highest header version among the inputs.
    let version = sources
        .iter()
        .map(|d| d.version.as_str())
        .max()
        .unwrap_or("1.7")
        .to_owned();
    let mut target = PdfDocument::new_empty(&version);

    let mut all_page_ids: Vec<ObjectId> = Vec::new();
    for source in sources {
        let page_ids = source
            .collect_page_ids()
            .ok_or_else(|| PdfError::Structure("a source document has no page tree".into()))?;
        let mut roots = Vec::with_capacity(page_ids.len());
        for page_id in page_ids {
            let mut dict = effective_page_dict(source, page_id)?;
            dict.remove("Parent");
            roots.push((page_id, PdfObject::Dictionary(dict)));
        }
        // One copy pass per source document: pages of the same document keep
        // sharing resources; documents never share objects with each other.
        let new_ids = copy_objects_into(&mut target, source, &roots)?;
        all_page_ids.extend(new_ids);
    }

    install_catalog(&mut target, &all_page_ids)?;

    // Take /Info from the first source that has one.
    for source in sources {
        if let Some(info_id) = source.info_ref() {
            if let Some(info) = source.resolve(info_id) {
                let ids = copy_objects_into(&mut target, source, &[(info_id, info.clone())])?;
                if let Some(&new_info) = ids.first() {
                    target.set_trailer_key("Info", PdfObject::Reference(new_info));
                }
                break;
            }
        }
    }
    Ok(target)
}

/// Convenience: merge files from paths.
pub fn merge_files(paths: &[impl AsRef<std::path::Path>]) -> Result<PdfDocument> {
    let docs: Vec<PdfDocument> = paths
        .iter()
        .map(PdfDocument::from_path)
        .collect::<Result<_>>()?;
    merge_documents(&docs.iter().collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_tree::pages;
    use crate::page_tree::test_support::nested_doc;

    #[test]
    fn merges_documents_in_order() {
        let a = nested_doc(2);
        let b = nested_doc(3);
        let merged = merge_documents(&[&a, &b]).unwrap();
        let pages = pages(&merged).unwrap();
        assert_eq!(pages.len(), 5);
        let markers: Vec<i64> = pages
            .iter()
            .map(|p| p.effective["PageIndexMarker"].as_i64().unwrap())
            .collect();
        assert_eq!(markers, vec![0, 1, 0, 1, 2]);
        // Inherited attributes survive the merge.
        assert_eq!(pages[0].effective["Rotate"].as_i64(), Some(90));

        // Round-trips through the writer.
        let bytes = merged.to_bytes().unwrap();
        let reread = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(reread.page_count(), Some(5));
    }

    #[test]
    fn merge_rejects_empty_input() {
        assert!(merge_documents(&[]).is_err());
    }

    #[test]
    fn merges_real_fixtures() {
        let simple =
            PdfDocument::from_bytes(include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let two =
            PdfDocument::from_bytes(include_bytes!("../../../fixtures/two_pages.pdf")).unwrap();
        let merged = merge_documents(&[&simple, &two]).unwrap();
        assert_eq!(merged.page_count(), Some(3));
        let bytes = merged.to_bytes().unwrap();
        assert_eq!(
            PdfDocument::from_bytes(&bytes).unwrap().page_count(),
            Some(3)
        );
    }
}
