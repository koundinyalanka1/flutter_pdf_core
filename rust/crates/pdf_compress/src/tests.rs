use super::*;

fn r(n: u32) -> PdfObject {
    PdfObject::Reference(ObjectId::new(n, 0))
}

fn dict(entries: Vec<(&str, PdfObject)>) -> Dictionary {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

fn name(n: &str) -> PdfObject {
    PdfObject::Name(n.into())
}

fn int(v: i64) -> PdfObject {
    PdfObject::Integer(v)
}

/// A page of 200 × 150 points showing a 1200 × 900 photo-like picture with
/// a soft mask, plus a content stream stored plain.
fn photo_pdf() -> PdfDocument {
    let (w, h) = (1200usize, 900usize);
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut seed = 7u32;
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let noise = (seed >> 24) as i32 % 24;
            rgb.push(((x * 255 / w) as i32 + noise).clamp(0, 255) as u8);
            rgb.push(((y * 255 / h) as i32 + noise).clamp(0, 255) as u8);
            rgb.push((((x + y) % 256) as i32).clamp(0, 255) as u8);
        }
    }
    let mut doc = PdfDocument::new_empty("1.4");
    let mut set = |n: u32, v: PdfObject| doc.set_object(ObjectId::new(n, 0), v);
    set(1, PdfObject::Dictionary(dict(vec![("Type", name("Catalog")), ("Pages", r(2))])));
    set(2, PdfObject::Dictionary(dict(vec![("Type", name("Pages")), ("Kids", PdfObject::Array(vec![r(3)])), ("Count", int(1))])));
    set(
        3,
        PdfObject::Dictionary(dict(vec![
            ("Type", name("Page")),
            ("Parent", r(2)),
            ("MediaBox", PdfObject::Array(vec![int(0), int(0), int(200), int(150)])),
            ("Contents", r(4)),
            ("Resources", PdfObject::Dictionary(dict(vec![("XObject", PdfObject::Dictionary(dict(vec![("Im0", r(5))])))]))),
        ])),
    );
    let mut content = b"q 200 0 0 150 0 0 cm /Im0 Do Q\n".to_vec();
    // Padding a reader ignores, so the stream is worth compressing.
    content.extend(std::iter::repeat_n(b"% a comment that repeats\n".to_vec(), 20).flatten());
    set(4, PdfObject::Stream(PdfStream::new(Dictionary::new(), content)));
    let image = dict(vec![
        ("Type", name("XObject")),
        ("Subtype", name("Image")),
        ("Width", int(w as i64)),
        ("Height", int(h as i64)),
        ("ColorSpace", name("DeviceRGB")),
        ("BitsPerComponent", int(8)),
        ("SMask", r(6)),
        ("Filter", name("FlateDecode")),
    ]);
    set(5, PdfObject::Stream(PdfStream::new(image, deflate(&rgb).unwrap())));
    let alpha: Vec<u8> = (0..w * h).map(|i| if (i % w) < w / 2 { 255 } else { 128 }).collect();
    let mask = dict(vec![
        ("Type", name("XObject")),
        ("Subtype", name("Image")),
        ("Width", int(w as i64)),
        ("Height", int(h as i64)),
        ("ColorSpace", name("DeviceGray")),
        ("BitsPerComponent", int(8)),
        ("Filter", name("FlateDecode")),
    ]);
    set(6, PdfObject::Stream(PdfStream::new(mask, deflate(&alpha).unwrap())));
    doc.set_trailer_key("Root", r(1));
    doc
}

#[test]
fn photos_are_scaled_to_the_page_and_reencoded() {
    let original = photo_pdf().to_bytes().unwrap();
    let options = Options { dpi: 150.0, quality: 70 };
    let (out, report) = compress_bytes(&original, "", &options).unwrap();
    assert_eq!(report, Report { images: 2, recompressed: 1, streams: 1 });
    assert!(out.len() * 4 < original.len(), "{} -> {}", original.len(), out.len());
    let doc = PdfDocument::from_bytes(&out).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    let page = doc.collect_page_ids().unwrap()[0];
    let resources = doc.resolve_dict(doc.resolve(page).unwrap().as_dict().unwrap().get("Resources").unwrap()).unwrap();
    let xobjects = doc.resolve_dict(resources.get("XObject").unwrap()).unwrap();
    let PdfObject::Stream(image) = doc.resolve_value(xobjects.get("Im0").unwrap()) else { panic!() };
    // 200 points at 150 dpi is 417 pixels.
    assert_eq!(image.dictionary.get("Width"), Some(&int(417)));
    assert_eq!(image.dictionary.get("Height"), Some(&int(313)));
    assert_eq!(image.dictionary.get("Filter"), Some(&name("DCTDecode")));
    // The mask keeps its exact pixels, and still applies.
    let PdfObject::Stream(mask) = doc.resolve_value(image.dictionary.get("SMask").unwrap()) else { panic!() };
    assert_eq!(mask.dictionary.get("Filter"), Some(&name("FlateDecode")));
    assert_eq!(mask.dictionary.get("Width"), Some(&int(1200)));
    // It still draws: the left half opaque, the right half see-through.
    let rendered = pdf_render::render_page(&doc, 0, pdf_render::RenderOptions::default()).unwrap();
    let px = |x: usize, y: usize| {
        let i = (y * rendered.width as usize + x) * 4;
        rendered.pixels[i..i + 3].to_vec()
    };
    let (left, right) = (px(20, 75), px(180, 75));
    assert!(left[2] < 200, "the picture shows on the left: {left:?}");
    assert!(right.iter().all(|v| *v > 100), "lighter where half transparent: {right:?}");
}

#[test]
fn the_compact_writer_round_trips() {
    let doc = photo_pdf();
    let bytes = write_compact(&doc).unwrap();
    assert!(bytes.windows(11).any(|w| w == b"/Type /XRef"));
    assert!(!bytes.windows(6).any(|w| w == b"\nxref\n"), "no table");
    let back = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(back.recovery_warnings.is_empty());
    assert_eq!(back.page_count(), Some(1));
    for id in doc.objects.keys() {
        let (a, b) = (doc.resolve(*id).unwrap(), back.resolve(*id).unwrap());
        match (a, b) {
            (PdfObject::Stream(x), PdfObject::Stream(y)) => assert_eq!(x.data, y.data),
            _ => assert_eq!(a, b, "object {id:?}"),
        }
    }
}

#[test]
fn a_file_that_cannot_shrink_is_returned_as_it_was() {
    let original = include_bytes!("../../../fixtures/simple.pdf");
    let (out, _) = compress_bytes(original, "", &Options::default()).unwrap();
    assert!(out.len() <= original.len());
    assert_eq!(PdfDocument::from_bytes(&out).unwrap().page_count(), Some(1));
}

#[test]
fn an_encrypted_file_stays_encrypted() {
    let original = include_bytes!("../../../fixtures/aes128_latin1_password.pdf");
    let mut doc = PdfDocument::from_bytes_with_password(original, "Pässwörd1").unwrap();
    compress(&mut doc, &Options::default());
    let bytes = pdf_core::crypt::encrypt_to_bytes(&doc, "Pässwörd1", "Pässwörd1").unwrap();
    assert!(PdfDocument::from_bytes(&bytes).is_err());
    assert_eq!(PdfDocument::from_bytes_with_password(&bytes, "Pässwörd1").unwrap().page_count(), Some(1));
}

#[test]
fn shrinking_averages_areas() {
    let samples = [0u8, 0, 255, 255, 0, 0, 255, 255];
    assert_eq!(shrink(&samples, 4, 2, 1, 2, 1), vec![0, 255]);
    assert_eq!(shrink(&samples, 4, 2, 1, 1, 1), vec![128]);
}

#[test]
fn pages_become_pictures() {
    let doc = photo_pdf();
    let png = page_image(&doc, 0, 72.0, PageImage::Png).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let jpeg = page_image(&doc, 0, 144.0, PageImage::Jpeg(80)).unwrap();
    assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    let mut decoder = pdf_render_jpeg(&jpeg);
    decoder.decode().unwrap();
    let info = decoder.info().unwrap();
    assert_eq!((info.width, info.height), (400, 300));
}

fn pdf_render_jpeg(data: &[u8]) -> jpeg_decoder::Decoder<&[u8]> {
    jpeg_decoder::Decoder::new(data)
}
