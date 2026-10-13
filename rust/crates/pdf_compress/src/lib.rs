//! Making a PDF smaller.
//!
//! * Pictures larger than the chosen resolution needs are scaled down and
//!   re-encoded as JPEG at the chosen quality (with the encoder in
//!   `pdf_render`), when that makes them smaller. Masks, black-and-white
//!   scans (CCITT, JBIG2) and pictures with colour-key transparency keep
//!   their exact pixels.
//! * Streams stored uncompressed or only ASCII-encoded are Flate-compressed.
//! * The file is written compactly: objects that are not streams are packed
//!   into compressed object streams, indexed by a cross-reference stream,
//!   and what nothing refers to is left out.
//!
//! The result is a new file: a signed document's signatures do not survive
//! it, so callers keep the original.

use std::collections::BTreeSet;
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use pdf_core::document::PdfDocument;
use pdf_core::error::Result;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_core::writer::PdfWriter;
use pdf_render::image::decode_image;

/// How hard to compress.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The resolution pictures are kept at, for the document's largest page.
    pub dpi: f64,
    /// JPEG quality, 1-100.
    pub quality: u8,
}

impl Default for Options {
    fn default() -> Self {
        Options { dpi: 150.0, quality: 70 }
    }
}

/// What was done.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Pictures in the document.
    pub images: usize,
    /// Pictures re-encoded smaller.
    pub recompressed: usize,
    /// Other streams compressed.
    pub streams: usize,
}

/// Pictures this small are left alone.
const SMALL_IMAGE: usize = 8 * 1024;

/// Compress a document's pictures and streams in place.
pub fn compress(doc: &mut PdfDocument, options: &Options) -> Report {
    doc.garbage_collect();
    let mut report = Report::default();
    let largest_page = (0..doc.page_count().unwrap_or(0) as usize)
        .filter_map(|i| pdf_render::page_size_points(doc, i).ok())
        .map(|(w, h)| w.max(h))
        .fold(0.0, f64::max);
    let largest_page = if largest_page > 0.0 { largest_page } else { 842.0 };
    let max_side = ((largest_page / 72.0 * options.dpi.clamp(36.0, 1200.0)).ceil() as usize).max(64);

    // Masks keep their exact pixels.
    let mut masks = BTreeSet::new();
    for object in doc.objects.values() {
        if let PdfObject::Stream(stream) = &object.value {
            for key in ["SMask", "Mask"] {
                if let Some(id) = stream.dictionary.get(key).and_then(PdfObject::as_ref) {
                    masks.insert(id);
                }
            }
        }
    }

    let ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    for id in ids {
        let Some(PdfObject::Stream(stream)) = doc.objects.get(&id).map(|o| &o.value) else { continue };
        if is_image(&stream.dictionary) {
            report.images += 1;
            if masks.contains(&id) {
                continue;
            }
            if let Some(smaller) = recompress(doc, stream, max_side, options.quality) {
                doc.set_object(id, PdfObject::Stream(smaller));
                report.recompressed += 1;
                continue;
            }
        }
        let Some(PdfObject::Stream(stream)) = doc.objects.get(&id).map(|o| &o.value) else { continue };
        if let Some(packed) = flate(doc, stream) {
            doc.set_object(id, PdfObject::Stream(packed));
            report.streams += 1;
        }
    }
    report
}

fn is_image(dict: &Dictionary) -> bool {
    dict.get("Subtype").and_then(PdfObject::as_name) == Some("Image")
}

fn filters(dict: &Dictionary) -> Vec<String> {
    match dict.get("Filter") {
        Some(PdfObject::Name(n)) => vec![n.clone()],
        Some(PdfObject::Array(items)) => items.iter().filter_map(|i| i.as_name().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

/// A picture re-encoded as a smaller JPEG, or None to keep it.
fn recompress(doc: &PdfDocument, stream: &PdfStream, max_side: usize, quality: u8) -> Option<PdfStream> {
    let dict = &stream.dictionary;
    if stream.data.len() < SMALL_IMAGE
        || matches!(dict.get("ImageMask"), Some(PdfObject::Bool(true)))
        || matches!(dict.get("Mask"), Some(PdfObject::Array(_)))
        || dict.get("BitsPerComponent").and_then(PdfObject::as_i64).is_some_and(|b| b < 8)
        || filters(dict).iter().any(|f| matches!(f.as_str(), "JBIG2Decode" | "CCITTFaxDecode" | "CCF"))
    {
        return None;
    }
    let image = decode_image(doc, stream)?;
    if image.is_stencil || image.rgb.is_empty() {
        return None;
    }
    let channels = image.channels();
    let (w, h) = (image.width, image.height);
    let scale = (max_side as f64 / w.max(h) as f64).min(1.0);
    let (nw, nh) = (((w as f64 * scale).round() as usize).max(1), ((h as f64 * scale).round() as usize).max(1));
    let samples = if (nw, nh) == (w, h) { image.rgb } else { shrink(&image.rgb, w, h, channels, nw, nh) };
    let jpeg = pdf_render::encode_jpeg(&samples, nw as u32, nh as u32, channels as u8, quality)?;
    if jpeg.len() * 10 >= stream.data.len() * 9 {
        return None;
    }
    let mut new = dict.clone();
    for key in ["Filter", "DecodeParms", "DP", "F", "Decode", "D", "Length", "SMaskInData", "ColorSpace", "CS", "BitsPerComponent", "BPC", "Width", "W", "Height", "H"] {
        new.remove(key);
    }
    new.insert("Filter".into(), PdfObject::Name("DCTDecode".into()));
    new.insert("ColorSpace".into(), PdfObject::Name(if channels == 1 { "DeviceGray" } else { "DeviceRGB" }.into()));
    new.insert("BitsPerComponent".into(), PdfObject::Integer(8));
    new.insert("Width".into(), PdfObject::Integer(nw as i64));
    new.insert("Height".into(), PdfObject::Integer(nh as i64));
    Some(PdfStream::new(new, jpeg))
}

/// Area-average downscaling.
fn shrink(samples: &[u8], w: usize, h: usize, channels: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * channels];
    let mut sums = vec![0u64; channels];
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h).div_ceil(nh).min(h).max(y * h / nh + 1));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w).div_ceil(nw).min(w).max(x * w / nw + 1));
            sums.iter_mut().for_each(|s| *s = 0);
            for sy in y0..y1 {
                let row = &samples[(sy * w + x0) * channels..(sy * w + x1) * channels];
                for px in row.chunks_exact(channels) {
                    for (s, v) in sums.iter_mut().zip(px) {
                        *s += u64::from(*v);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            for (c, s) in sums.iter().enumerate() {
                out[(y * nw + x) * channels + c] = ((s + n / 2) / n) as u8;
            }
        }
    }
    out
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

/// A stream stored plain or only ASCII-encoded, Flate-compressed.
fn flate(doc: &PdfDocument, stream: &PdfStream) -> Option<PdfStream> {
    let dict = &stream.dictionary;
    let kind = dict.get("Type").and_then(PdfObject::as_name);
    if matches!(kind, Some("XRef" | "ObjStm" | "Metadata")) || stream.data.len() < 128 {
        return None;
    }
    let list = filters(dict);
    if !list.iter().all(|f| matches!(f.as_str(), "ASCIIHexDecode" | "AHx" | "ASCII85Decode" | "A85")) {
        return None;
    }
    let plain = if list.is_empty() { stream.data.clone() } else { doc.stream_data(stream).ok()? };
    let packed = deflate(&plain)?;
    if packed.len() >= stream.data.len() {
        return None;
    }
    let mut new = dict.clone();
    new.remove("DecodeParms");
    new.remove("DP");
    new.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
    Some(PdfStream::new(new, packed))
}

/// Objects packed into each object stream.
const PER_OBJECT_STREAM: usize = 100;

/// The document written compactly: streams as they are, every other
/// object in compressed object streams, and a cross-reference stream.
/// For a document that is not encrypted.
pub fn write_compact(doc: &PdfDocument) -> Result<Vec<u8>> {
    let root = doc.root_ref().ok_or_else(|| pdf_core::error::PdfError::Structure("no /Root".into()))?;
    let mut out = format!("%PDF-{}\n", doc.version.as_str().max("1.5")).into_bytes();
    out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");
    let mut next = doc.max_object_number() + 1;
    // (number, type, field 2, field 3) for the cross-reference stream.
    let mut entries: Vec<(u32, u8, u64, u16)> = Vec::new();
    let mut loose: Vec<ObjectId> = Vec::new();
    for (id, object) in &doc.objects {
        match &object.value {
            PdfObject::Stream(stream) => {
                entries.push((id.number, 1, out.len() as u64, id.generation));
                writeln!(out, "{} {} obj", id.number, id.generation)?;
                out.extend_from_slice(&PdfWriter::serialize_object(&PdfObject::Stream(stream.clone()))?);
                out.extend_from_slice(b"\nendobj\n");
            }
            _ if id.generation == 0 => loose.push(*id),
            value => {
                entries.push((id.number, 1, out.len() as u64, id.generation));
                writeln!(out, "{} {} obj", id.number, id.generation)?;
                out.extend_from_slice(&PdfWriter::serialize_object(value)?);
                out.extend_from_slice(b"\nendobj\n");
            }
        }
    }
    for chunk in loose.chunks(PER_OBJECT_STREAM) {
        let number = next;
        next += 1;
        let mut header = String::new();
        let mut body = Vec::new();
        for (index, id) in chunk.iter().enumerate() {
            header.push_str(&format!("{} {} ", id.number, body.len()));
            body.extend_from_slice(&PdfWriter::serialize_object(&doc.objects[id].value)?);
            body.push(b'\n');
            entries.push((id.number, 2, u64::from(number), index as u16));
        }
        let first = header.len();
        let mut data = header.into_bytes();
        data.extend_from_slice(&body);
        let packed = deflate(&data).unwrap_or_else(|| data.clone());
        let mut dict = Dictionary::new();
        dict.insert("Type".into(), PdfObject::Name("ObjStm".into()));
        dict.insert("N".into(), PdfObject::Integer(chunk.len() as i64));
        dict.insert("First".into(), PdfObject::Integer(first as i64));
        dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
        entries.push((number, 1, out.len() as u64, 0));
        writeln!(out, "{number} 0 obj")?;
        out.extend_from_slice(&PdfWriter::serialize_object(&PdfObject::Stream(PdfStream::new(dict, packed)))?);
        out.extend_from_slice(b"\nendobj\n");
    }
    let own = next;
    let at = out.len() as u64;
    entries.push((own, 1, at, 0));
    entries.push((0, 0, 0, 0xFFFF));
    entries.sort_by_key(|e| e.0);
    let mut data = Vec::new();
    let mut index = Vec::new();
    let mut first = 0;
    while first < entries.len() {
        let mut end = first + 1;
        while end < entries.len() && entries[end].0 == entries[end - 1].0 + 1 {
            end += 1;
        }
        index.push(PdfObject::Integer(i64::from(entries[first].0)));
        index.push(PdfObject::Integer((end - first) as i64));
        for &(_, kind, field2, field3) in &entries[first..end] {
            data.push(kind);
            data.extend_from_slice(&field2.to_be_bytes()[3..]);
            data.extend_from_slice(&field3.to_be_bytes());
        }
        first = end;
    }
    let mut dict = Dictionary::new();
    dict.insert("Type".into(), PdfObject::Name("XRef".into()));
    dict.insert("Size".into(), PdfObject::Integer(i64::from(own) + 1));
    dict.insert("Index".into(), PdfObject::Array(index));
    dict.insert("W".into(), PdfObject::Array(vec![PdfObject::Integer(1), PdfObject::Integer(5), PdfObject::Integer(2)]));
    dict.insert("Root".into(), PdfObject::Reference(root));
    for key in ["Info", "ID"] {
        if let Some(v) = doc.xref.trailer.get(key) {
            dict.insert(key.into(), v.clone());
        }
    }
    let packed = deflate(&data).unwrap_or_else(|| data.clone());
    dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
    writeln!(out, "{own} 0 obj")?;
    out.extend_from_slice(&PdfWriter::serialize_object(&PdfObject::Stream(PdfStream::new(dict, packed)))?);
    write!(out, "\nendobj\nstartxref\n{at}\n%%EOF\n")?;
    Ok(out)
}

/// Compress a PDF's bytes; the result is the smaller of the compressed file
/// and the original. An encrypted file is encrypted again with its password.
pub fn compress_bytes(bytes: &[u8], password: &str, options: &Options) -> Result<(Vec<u8>, Report)> {
    let mut doc = PdfDocument::from_bytes_with_password(bytes, password)?;
    let report = compress(&mut doc, options);
    let out = if doc.was_encrypted {
        pdf_core::crypt::encrypt_to_bytes(&doc, password, password)?
    } else {
        write_compact(&doc)?
    };
    Ok(if out.len() < bytes.len() { (out, report) } else { (bytes.to_vec(), Report { recompressed: 0, streams: 0, ..report }) })
}

/// A page saved as a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageImage {
    Png,
    /// With its quality, 1-100.
    Jpeg(u8),
}

/// Page `index` (0-based) as a PNG or JPEG at `dpi`.
pub fn page_image(doc: &PdfDocument, index: usize, dpi: f64, format: PageImage) -> Result<Vec<u8>> {
    let scale = dpi.clamp(18.0, 600.0) / 72.0;
    let options = pdf_render::RenderOptions { size: pdf_render::RenderSize::Scale(scale), ..Default::default() };
    let page = pdf_render::render_page(doc, index, options)?;
    let encoded = match format {
        PageImage::Png => pdf_render::encode_rgba_as_png(&page.pixels, page.width, page.height),
        PageImage::Jpeg(quality) => pdf_render::encode_rgba_as_jpeg(&page.pixels, page.width, page.height, quality),
    };
    encoded.ok_or_else(|| pdf_core::error::PdfError::Structure("the page could not be encoded".into()))
}

#[cfg(test)]
mod tests;
