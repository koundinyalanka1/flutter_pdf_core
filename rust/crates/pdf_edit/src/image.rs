//! Pictures as image XObjects: JPEGs as they are, raw RGBA compressed with
//! its transparency as a soft mask.

use pdf_core::error::{PdfError, Result};
use pdf_core::object::PdfObject;
use pdf_core::stream::PdfStream;
use pdf_ops::compose::parse_jpeg;

use crate::content::{dict, flate_stream, name, numbers};
use crate::editor::Editor;
use crate::model::ImageSource;

/// The largest picture taken, in pixels on a side.
const MAX_SIDE: u32 = 8192;

/// An image added to the document, and its size in pixels.
pub fn add(editor: &mut Editor, source: &ImageSource) -> Result<(PdfObject, u32, u32)> {
    let bytes = std::fs::read(&source.path)?;
    if source.width == 0 || source.height == 0 {
        let info = parse_jpeg(&bytes)?;
        let space = match info.components {
            1 => "DeviceGray",
            3 => "DeviceRGB",
            4 => "DeviceCMYK",
            _ => return Err(PdfError::Unsupported("JPEG colour model")),
        };
        let mut d = dict(vec![
            ("Type", name("XObject")),
            ("Subtype", name("Image")),
            ("Width", PdfObject::Integer(i64::from(info.width))),
            ("Height", PdfObject::Integer(i64::from(info.height))),
            ("ColorSpace", name(space)),
            ("BitsPerComponent", PdfObject::Integer(8)),
            ("Filter", name("DCTDecode")),
        ]);
        if info.components == 4 && info.adobe {
            // Adobe's CMYK JPEGs are stored inverted.
            d.insert("Decode".into(), numbers(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]));
        }
        let id = editor.add(PdfObject::Stream(PdfStream::new(d, bytes)));
        return Ok((PdfObject::Reference(id), info.width, info.height));
    }
    let (w, h) = (source.width, source.height);
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(PdfError::Unsupported("picture larger than 8192 pixels on a side"));
    }
    let pixels = w as usize * h as usize;
    if bytes.len() < pixels * 4 {
        return Err(PdfError::Structure("picture has fewer pixels than its size says".into()));
    }
    let mut rgb = Vec::with_capacity(pixels * 3);
    let mut alpha = Vec::with_capacity(pixels);
    for px in bytes[..pixels * 4].chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
        alpha.push(px[3]);
    }
    let image = |space: &str, data: &[u8]| {
        flate_stream(
            dict(vec![
                ("Type", name("XObject")),
                ("Subtype", name("Image")),
                ("Width", PdfObject::Integer(i64::from(w))),
                ("Height", PdfObject::Integer(i64::from(h))),
                ("ColorSpace", name(space)),
                ("BitsPerComponent", PdfObject::Integer(8)),
            ]),
            data,
        )
    };
    let mut color = image("DeviceRGB", &rgb);
    if alpha.iter().any(|a| *a != 255) {
        let mask = editor.add(image("DeviceGray", &alpha));
        if let PdfObject::Stream(stream) = &mut color {
            stream.dictionary.insert("SMask".into(), PdfObject::Reference(mask));
        }
    }
    let id = editor.add(color);
    Ok((PdfObject::Reference(id), w, h))
}
