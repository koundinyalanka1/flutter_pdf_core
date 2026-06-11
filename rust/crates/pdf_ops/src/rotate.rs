//! Milestone 6 (part 1): page rotation and crop boxes.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{ObjectId, PdfObject};

use crate::page_tree::page_attribute;

/// A rectangle in default user-space units (points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn to_array(self) -> PdfObject {
        PdfObject::Array(vec![
            PdfObject::Real(self.x0),
            PdfObject::Real(self.y0),
            PdfObject::Real(self.x1),
            PdfObject::Real(self.y1),
        ])
    }

    pub fn from_object(object: &PdfObject) -> Option<Self> {
        let PdfObject::Array(items) = object else {
            return None;
        };
        if items.len() != 4 {
            return None;
        }
        let mut v = [0f64; 4];
        for (i, item) in items.iter().enumerate() {
            v[i] = match item {
                PdfObject::Integer(n) => *n as f64,
                PdfObject::Real(n) => *n,
                _ => return None,
            };
        }
        Some(Self {
            x0: v[0],
            y0: v[1],
            x1: v[2],
            y1: v[3],
        })
    }
}

fn normalize_degrees(degrees: i64) -> Result<i64> {
    let normalized = degrees.rem_euclid(360);
    if normalized % 90 != 0 {
        return Err(PdfError::Structure(format!(
            "rotation must be a multiple of 90 degrees, got {degrees}"
        )));
    }
    Ok(normalized)
}

fn page_id_at(doc: &PdfDocument, index: usize) -> Result<ObjectId> {
    doc.collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?
        .get(index)
        .copied()
        .ok_or(PdfError::PageIndex(index))
}

fn update_page_dict(
    doc: &mut PdfDocument,
    page_id: ObjectId,
    apply: impl FnOnce(&mut pdf_core::object::Dictionary),
) -> Result<()> {
    let mut dict = doc
        .resolve(page_id)
        .and_then(PdfObject::as_dict)
        .cloned()
        .ok_or_else(|| PdfError::Structure("page object is not a dictionary".into()))?;
    apply(&mut dict);
    doc.set_object(page_id, PdfObject::Dictionary(dict));
    Ok(())
}

/// Current effective rotation of a page (inherited, normalized to 0..360).
pub fn get_rotation(doc: &PdfDocument, index: usize) -> Result<i64> {
    let page_id = page_id_at(doc, index)?;
    Ok(page_attribute(doc, page_id, "Rotate")
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .rem_euclid(360))
}

/// Rotate one page by `delta` degrees relative to its current rotation.
pub fn rotate_page(doc: &mut PdfDocument, index: usize, delta: i64) -> Result<()> {
    let current = get_rotation(doc, index)?;
    let target = normalize_degrees(current + delta)?;
    let page_id = page_id_at(doc, index)?;
    update_page_dict(doc, page_id, |dict| {
        dict.insert("Rotate".into(), PdfObject::Integer(target));
    })
}

/// Set the absolute rotation of one page.
pub fn set_rotation(doc: &mut PdfDocument, index: usize, degrees: i64) -> Result<()> {
    let target = normalize_degrees(degrees)?;
    let page_id = page_id_at(doc, index)?;
    update_page_dict(doc, page_id, |dict| {
        dict.insert("Rotate".into(), PdfObject::Integer(target));
    })
}

/// Rotate every page by `delta` degrees.
pub fn rotate_all_pages(doc: &mut PdfDocument, delta: i64) -> Result<()> {
    let n = doc
        .collect_page_ids()
        .ok_or_else(|| PdfError::Structure("document has no page tree".into()))?
        .len();
    for index in 0..n {
        rotate_page(doc, index, delta)?;
    }
    Ok(())
}

/// Effective media box of a page (inherited; defaults to US Letter).
pub fn get_media_box(doc: &PdfDocument, index: usize) -> Result<Rect> {
    let page_id = page_id_at(doc, index)?;
    Ok(page_attribute(doc, page_id, "MediaBox")
        .as_ref()
        .and_then(Rect::from_object)
        .unwrap_or(Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 612.0,
            y1: 792.0,
        }))
}

/// Effective crop box (falls back to the media box per the PDF spec).
pub fn get_crop_box(doc: &PdfDocument, index: usize) -> Result<Rect> {
    let page_id = page_id_at(doc, index)?;
    if let Some(rect) = page_attribute(doc, page_id, "CropBox")
        .as_ref()
        .and_then(Rect::from_object)
    {
        return Ok(rect);
    }
    get_media_box(doc, index)
}

/// Set the crop box of one page.
pub fn set_crop_box(doc: &mut PdfDocument, index: usize, rect: Rect) -> Result<()> {
    if rect.x1 <= rect.x0 || rect.y1 <= rect.y0 {
        return Err(PdfError::Structure("crop box has no area".into()));
    }
    let page_id = page_id_at(doc, index)?;
    update_page_dict(doc, page_id, |dict| {
        dict.insert("CropBox".into(), rect.to_array());
    })
}

/// Set the media box of one page.
pub fn set_media_box(doc: &mut PdfDocument, index: usize, rect: Rect) -> Result<()> {
    if rect.x1 <= rect.x0 || rect.y1 <= rect.y0 {
        return Err(PdfError::Structure("media box has no area".into()));
    }
    let page_id = page_id_at(doc, index)?;
    update_page_dict(doc, page_id, |dict| {
        dict.insert("MediaBox".into(), rect.to_array());
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_tree::test_support::nested_doc;

    #[test]
    fn rotation_is_relative_to_inherited_value() {
        let mut doc = nested_doc(2);
        // Inherited rotation is 90 (set on the inner Pages node).
        assert_eq!(get_rotation(&doc, 0).unwrap(), 90);
        rotate_page(&mut doc, 0, 90).unwrap();
        assert_eq!(get_rotation(&doc, 0).unwrap(), 180);
        // The sibling page is untouched.
        assert_eq!(get_rotation(&doc, 1).unwrap(), 90);
        // Negative deltas and wrapping.
        rotate_page(&mut doc, 0, -270).unwrap();
        assert_eq!(get_rotation(&doc, 0).unwrap(), 270);
        assert!(rotate_page(&mut doc, 0, 45).is_err());
        assert!(rotate_page(&mut doc, 9, 90).is_err());
    }

    #[test]
    fn crop_box_defaults_to_media_box_and_can_be_set() {
        let mut doc = nested_doc(1);
        let media = get_media_box(&doc, 0).unwrap();
        assert_eq!(media.x1, 612.0);
        assert_eq!(get_crop_box(&doc, 0).unwrap(), media);

        let crop = Rect {
            x0: 10.0,
            y0: 10.0,
            x1: 300.0,
            y1: 400.0,
        };
        set_crop_box(&mut doc, 0, crop).unwrap();
        assert_eq!(get_crop_box(&doc, 0).unwrap(), crop);
        // Media box unchanged.
        assert_eq!(get_media_box(&doc, 0).unwrap(), media);
        // Degenerate rects rejected.
        assert!(set_crop_box(
            &mut doc,
            0,
            Rect {
                x0: 5.0,
                y0: 5.0,
                x1: 5.0,
                y1: 50.0
            }
        )
        .is_err());
    }
}
