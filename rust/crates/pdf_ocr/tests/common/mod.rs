//! Fixtures shared by the integration tests: a typeset page, the same page
//! as a scan, and character error rate.

#![allow(dead_code)]

use pdf_core::document::PdfDocument;
use pdf_core::filter::flate_encode;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_ocr::GrayImage;
use pdf_render::{render_page, RenderOptions, RenderSize};

pub const LINES: [&str; 4] = [
    "Invoice 2024-117 for Example Corp.",
    "Total due: $1,234.56 by March 3, 2025.",
    "Thank you for your business!",
    "Questions? Write to billing@example.com",
];

/// Install `page` as the only page of `doc`.
pub fn install_page(doc: &mut PdfDocument, mut page: Dictionary) {
    let pages = doc.add_object(PdfObject::Null);
    page.insert("Type".into(), PdfObject::Name("Page".into()));
    page.insert("Parent".into(), PdfObject::Reference(pages));
    page.insert(
        "MediaBox".into(),
        PdfObject::Array([0, 0, 612, 792].map(PdfObject::Integer).to_vec()),
    );
    let page_id = doc.add_object(PdfObject::Dictionary(page));
    doc.set_object(
        pages,
        PdfObject::Dictionary(Dictionary::from([
            ("Type".into(), PdfObject::Name("Pages".into())),
            (
                "Kids".into(),
                PdfObject::Array(vec![PdfObject::Reference(page_id)]),
            ),
            ("Count".into(), PdfObject::Integer(1)),
        ])),
    );
    let catalog = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Catalog".into())),
        ("Pages".into(), PdfObject::Reference(pages)),
    ])));
    doc.set_trailer_key("Root", PdfObject::Reference(catalog));
}

/// The lines typeset in Helvetica: a born-digital page.
pub fn typeset() -> PdfDocument {
    let mut content = String::from("BT /F1 16 Tf 22 TL 72 700 Td\n");
    for line in LINES {
        content += &format!("({}) Tj T*\n", line.replace('(', "\\(").replace(')', "\\)"));
    }
    content += "ET";
    let mut doc = PdfDocument::new_empty("1.7");
    let font = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Font".into())),
        ("Subtype".into(), PdfObject::Name("Type1".into())),
        ("BaseFont".into(), PdfObject::Name("Helvetica".into())),
        ("Encoding".into(), PdfObject::Name("WinAnsiEncoding".into())),
    ])));
    let content = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        content.into_bytes(),
    )));
    let mut page = Dictionary::new();
    page.insert(
        "Resources".into(),
        PdfObject::Dictionary(Dictionary::from([(
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "F1".into(),
                PdfObject::Reference(font),
            )])),
        )])),
    );
    page.insert("Contents".into(), PdfObject::Reference(content));
    install_page(&mut doc, page);
    doc
}

/// The same page as a scanner would deliver it: one grey image, no text.
pub fn scanned() -> PdfDocument {
    let source = typeset();
    let render = render_page(
        &source,
        0,
        RenderOptions {
            size: RenderSize::Scale(200.0 / 72.0),
            ..Default::default()
        },
    )
    .unwrap();
    let gray = GrayImage::from_rgba(
        render.width as usize,
        render.height as usize,
        &render.pixels,
    )
    .unwrap();
    let mut doc = PdfDocument::new_empty("1.7");
    let image = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::from([
            ("Type".into(), PdfObject::Name("XObject".into())),
            ("Subtype".into(), PdfObject::Name("Image".into())),
            ("Width".into(), PdfObject::Integer(gray.width as i64)),
            ("Height".into(), PdfObject::Integer(gray.height as i64)),
            ("ColorSpace".into(), PdfObject::Name("DeviceGray".into())),
            ("BitsPerComponent".into(), PdfObject::Integer(8)),
            ("Filter".into(), PdfObject::Name("FlateDecode".into())),
        ]),
        flate_encode(&gray.pixels),
    )));
    let content = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        b"q 612 0 0 792 0 0 cm /Im0 Do Q".to_vec(),
    )));
    let mut page = Dictionary::new();
    page.insert(
        "Resources".into(),
        PdfObject::Dictionary(Dictionary::from([(
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "Im0".into(),
                PdfObject::Reference(image),
            )])),
        )])),
    );
    page.insert("Contents".into(), PdfObject::Reference(content));
    install_page(&mut doc, page);
    doc
}

pub fn character_error_rate(truth: &str, read: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (truth.chars().collect(), read.chars().collect());
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(x != y)),
            );
        }
        previous = current;
    }
    previous[b.len()] as f64 / a.len() as f64
}

pub fn single_spaced(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The typeset page rendered to grey pixels at `dpi`.
pub fn page_image(dpi: f64) -> GrayImage {
    let render = render_page(
        &typeset(),
        0,
        RenderOptions {
            size: RenderSize::Scale(dpi / 72.0),
            ..Default::default()
        },
    )
    .unwrap();
    GrayImage::from_rgba(
        render.width as usize,
        render.height as usize,
        &render.pixels,
    )
    .unwrap()
}
