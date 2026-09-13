//! Milestone 13 (part 1): build PDFs from images.
//!
//! Scanned pages arrive as JPEGs. A JPEG is already a valid PDF image stream —
//! `DCTDecode` is baseline JPEG — so a page can embed the *exact* bytes the
//! camera produced with no decode/re-encode step. That keeps this module free
//! of any image codec: it only reads the JPEG's own SOF header to learn the
//! pixel dimensions and component count.
//!
//! ```no_run
//! # use pdf_ops::compose::{images_to_document, ComposeOptions};
//! let doc = images_to_document(&[std::fs::read("scan.jpg").unwrap()],
//!                              ComposeOptions::default()).unwrap();
//! doc.save_as("out.pdf").unwrap();
//! ```

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;

use crate::split::install_catalog;

/// A4 in PostScript points (1/72").
pub const A4: (f64, f64) = (595.276, 841.890);
/// US Letter in PostScript points.
pub const LETTER: (f64, f64) = (612.0, 792.0);

/// How an image is laid onto its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFit {
    /// Fixed page size; the image is scaled to fit inside it and centred,
    /// preserving aspect ratio (letterboxed).
    Contain,
    /// The page takes the image's own aspect ratio, scaled so its longest
    /// edge matches the longest edge of the configured page size. No bars.
    ImageAspect,
}

#[derive(Debug, Clone, Copy)]
pub struct ComposeOptions {
    /// Page box in points. Ignored for the sizing axis under
    /// [`PageFit::ImageAspect`], which still uses it as the target extent.
    pub page_size: (f64, f64),
    pub fit: PageFit,
    /// Margin in points applied on all four sides. Clamped so a page always
    /// keeps a positive drawable area.
    pub margin: f64,
}

impl Default for ComposeOptions {
    fn default() -> Self {
        Self {
            page_size: A4,
            fit: PageFit::ImageAspect,
            margin: 0.0,
        }
    }
}

/// What the SOF marker told us about a JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// 1 = greyscale, 3 = YCbCr/RGB, 4 = CMYK/YCCK.
    pub components: u8,
    pub bits_per_component: u8,
    /// True when an Adobe APP14 marker is present. CMYK JPEGs written by
    /// Adobe tools store inverted ink values, which needs a /Decode array.
    pub adobe: bool,
}

impl JpegInfo {
    fn color_space(&self) -> Result<&'static str> {
        match self.components {
            1 => Ok("DeviceGray"),
            3 => Ok("DeviceRGB"),
            4 => Ok("DeviceCMYK"),
            n => Err(PdfError::Structure(format!(
                "unsupported JPEG component count: {n}"
            ))),
        }
    }
}

/// Read width/height/components out of a JPEG's SOF segment.
///
/// Walks the marker chain rather than trusting any single offset, so it works
/// for files carrying EXIF, ICC profiles or thumbnails ahead of the frame.
pub fn parse_jpeg(data: &[u8]) -> Result<JpegInfo> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(PdfError::Structure("not a JPEG (missing SOI)".into()));
    }

    let mut adobe = false;
    let mut i = 2usize;
    while i + 3 < data.len() {
        if data[i] != 0xFF {
            // Pad bytes are legal between segments; skip them.
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        match marker {
            // Standalone markers: no length field.
            0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7 => continue,
            // Start of scan — entropy-coded data follows, no frame header left.
            0xDA => break,
            _ => {}
        }
        if i + 1 >= data.len() {
            break;
        }
        let length = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if length < 2 || i + length > data.len() {
            return Err(PdfError::Structure("truncated JPEG segment".into()));
        }
        let payload = &data[i + 2..i + length];

        if marker == 0xEE && payload.starts_with(b"Adobe") {
            adobe = true;
        }

        // SOF0..SOF15, excluding DHT (C4), JPG (C8) and DAC (CC).
        let is_sof = (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC;
        if is_sof {
            if payload.len() < 6 {
                return Err(PdfError::Structure("truncated JPEG frame header".into()));
            }
            return Ok(JpegInfo {
                bits_per_component: payload[0],
                height: u16::from_be_bytes([payload[1], payload[2]]) as u32,
                width: u16::from_be_bytes([payload[3], payload[4]]) as u32,
                components: payload[5],
                adobe,
            });
        }
        i += length;
    }
    Err(PdfError::Structure("no JPEG frame header found".into()))
}

/// Build a document with one page per image.
pub fn images_to_document(images: &[Vec<u8>], options: ComposeOptions) -> Result<PdfDocument> {
    if images.is_empty() {
        return Err(PdfError::Structure("no images supplied".into()));
    }

    let mut doc = PdfDocument::new_empty("1.7");
    let mut page_ids = Vec::with_capacity(images.len());
    for data in images {
        page_ids.push(add_image_page(&mut doc, data, options)?);
    }
    install_catalog(&mut doc, &page_ids)?;
    Ok(doc)
}

/// Append one image as a new page. Returns the page object id.
pub fn add_image_page(
    doc: &mut PdfDocument,
    jpeg: &[u8],
    options: ComposeOptions,
) -> Result<ObjectId> {
    let info = parse_jpeg(jpeg)?;
    if info.width == 0 || info.height == 0 {
        return Err(PdfError::Structure("JPEG has zero extent".into()));
    }

    let (page_w, page_h, draw_w, draw_h, offset_x, offset_y) = layout(&info, options);

    // --- image XObject -----------------------------------------------------
    let mut image_dict = Dictionary::new();
    image_dict.insert("Type".into(), PdfObject::Name("XObject".into()));
    image_dict.insert("Subtype".into(), PdfObject::Name("Image".into()));
    image_dict.insert("Width".into(), PdfObject::Integer(info.width as i64));
    image_dict.insert("Height".into(), PdfObject::Integer(info.height as i64));
    image_dict.insert(
        "ColorSpace".into(),
        PdfObject::Name(info.color_space()?.into()),
    );
    image_dict.insert(
        "BitsPerComponent".into(),
        PdfObject::Integer(info.bits_per_component.max(8) as i64),
    );
    image_dict.insert("Filter".into(), PdfObject::Name("DCTDecode".into()));
    if info.components == 4 && info.adobe {
        // Adobe CMYK JPEGs are stored inverted.
        image_dict.insert(
            "Decode".into(),
            PdfObject::Array(
                (0..4)
                    .flat_map(|_| [PdfObject::Integer(1), PdfObject::Integer(0)])
                    .collect(),
            ),
        );
    }
    let image_id = doc.add_object(PdfObject::Stream(PdfStream::new(
        image_dict,
        jpeg.to_vec(),
    )));

    // --- content stream ----------------------------------------------------
    // `cm` maps the image's unit square onto the drawable rectangle.
    let content = format!(
        "q\n{draw_w:.4} 0 0 {draw_h:.4} {offset_x:.4} {offset_y:.4} cm\n/Im0 Do\nQ\n"
    );
    let mut content_dict = Dictionary::new();
    content_dict.insert("Length".into(), PdfObject::Integer(content.len() as i64));
    let content_id = doc.add_object(PdfObject::Stream(PdfStream::new(
        content_dict,
        content.into_bytes(),
    )));

    // --- page --------------------------------------------------------------
    let mut xobjects = Dictionary::new();
    xobjects.insert("Im0".into(), PdfObject::Reference(image_id));
    let mut resources = Dictionary::new();
    resources.insert("XObject".into(), PdfObject::Dictionary(xobjects));

    let mut page = Dictionary::new();
    page.insert("Type".into(), PdfObject::Name("Page".into()));
    page.insert(
        "MediaBox".into(),
        PdfObject::Array(vec![
            PdfObject::Integer(0),
            PdfObject::Integer(0),
            PdfObject::Real(page_w),
            PdfObject::Real(page_h),
        ]),
    );
    page.insert("Resources".into(), PdfObject::Dictionary(resources));
    page.insert("Contents".into(), PdfObject::Reference(content_id));
    Ok(doc.add_object(PdfObject::Dictionary(page)))
}

/// Page box and the rectangle the image is drawn into, in points.
fn layout(info: &JpegInfo, options: ComposeOptions) -> (f64, f64, f64, f64, f64, f64) {
    let aspect = info.width as f64 / info.height as f64;
    let (target_w, target_h) = options.page_size;

    let (page_w, page_h) = match options.fit {
        PageFit::Contain => (target_w, target_h),
        PageFit::ImageAspect => {
            // Match the page to the image, scaled to the target's long edge.
            let long_edge = target_w.max(target_h);
            if aspect >= 1.0 {
                (long_edge, long_edge / aspect)
            } else {
                (long_edge * aspect, long_edge)
            }
        }
    };

    // Never let the margin eat the whole page.
    let margin = options.margin.max(0.0).min(page_w.min(page_h) / 2.0 - 1.0).max(0.0);
    let avail_w = (page_w - margin * 2.0).max(1.0);
    let avail_h = (page_h - margin * 2.0).max(1.0);

    let scale = (avail_w / aspect).min(avail_h);
    let draw_w = scale * aspect;
    let draw_h = scale;

    (
        page_w,
        page_h,
        draw_w,
        draw_h,
        (page_w - draw_w) / 2.0,
        (page_h - draw_h) / 2.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal but structurally valid baseline JPEG header: SOI, APP0/JFIF,
    /// SOF0 (1 component, 8x4), SOS. Enough to exercise the marker walk.
    fn tiny_jpeg(width: u16, height: u16, components: u8) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // APP0 segment we must skip over.
        out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        out.extend_from_slice(b"JFIF\0");
        out.extend_from_slice(&[0u8; 9]);
        // SOF0
        let frame_len = 8 + 3 * (components as u16 - 1);
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&frame_len.to_be_bytes());
        out.push(8); // precision
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.push(components);
        for c in 0..components {
            out.extend_from_slice(&[c + 1, 0x11, 0]);
        }
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn reads_dimensions_past_app0() {
        let info = parse_jpeg(&tiny_jpeg(1200, 900, 3)).unwrap();
        assert_eq!(info.width, 1200);
        assert_eq!(info.height, 900);
        assert_eq!(info.components, 3);
        assert_eq!(info.bits_per_component, 8);
        assert!(!info.adobe);
    }

    #[test]
    fn rejects_non_jpeg() {
        assert!(parse_jpeg(b"%PDF-1.7").is_err());
    }

    #[test]
    fn builds_one_page_per_image() {
        let images = vec![tiny_jpeg(800, 600, 3), tiny_jpeg(600, 800, 1)];
        let doc = images_to_document(&images, ComposeOptions::default()).unwrap();
        assert_eq!(doc.page_count(), Some(2));

        let bytes = doc.to_bytes().unwrap();
        let reparsed = PdfDocument::from_bytes(&bytes).unwrap();
        assert_eq!(reparsed.page_count(), Some(2));
    }

    #[test]
    fn image_aspect_page_matches_image_orientation() {
        let landscape = images_to_document(
            &[tiny_jpeg(1000, 500, 3)],
            ComposeOptions {
                fit: PageFit::ImageAspect,
                ..Default::default()
            },
        )
        .unwrap();
        let page_id = landscape.collect_page_ids().unwrap()[0];
        let dict = landscape.resolve(page_id).unwrap().as_dict().unwrap();
        let media = match dict.get("MediaBox") {
            Some(PdfObject::Array(items)) => items,
            other => panic!("expected MediaBox array, got {other:?}"),
        };
        let w = as_f64(&media[2]);
        let h = as_f64(&media[3]);
        assert!(w > h, "landscape image should produce a landscape page");
        assert!((w / h - 2.0).abs() < 0.01);
    }

    #[test]
    fn contain_keeps_the_requested_page_size() {
        let doc = images_to_document(
            &[tiny_jpeg(1000, 500, 3)],
            ComposeOptions {
                page_size: A4,
                fit: PageFit::Contain,
                margin: 36.0,
            },
        )
        .unwrap();
        let page_id = doc.collect_page_ids().unwrap()[0];
        let dict = doc.resolve(page_id).unwrap().as_dict().unwrap();
        let media = match dict.get("MediaBox") {
            Some(PdfObject::Array(items)) => items,
            other => panic!("expected MediaBox array, got {other:?}"),
        };
        assert!((as_f64(&media[2]) - A4.0).abs() < 0.01);
        assert!((as_f64(&media[3]) - A4.1).abs() < 0.01);
    }

    fn as_f64(object: &PdfObject) -> f64 {
        match object {
            PdfObject::Integer(v) => *v as f64,
            PdfObject::Real(v) => *v,
            other => panic!("expected number, got {other:?}"),
        }
    }
}
