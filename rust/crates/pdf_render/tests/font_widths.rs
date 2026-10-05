//! Non-private equivalent of a receipt which used an indirect CID /W array.
use pdf_core::document::PdfDocument;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::parser::Parser;
use pdf_core::stream::PdfStream;
use pdf_render::{render_page, RenderOptions};
use pdf_text::layout::extract_page_layout;

fn object(source: &str) -> PdfObject {
    Parser::new(source.as_bytes()).parse_object().unwrap()
}

fn document(indirect: bool) -> PdfDocument {
    let mut doc = PdfDocument::new_empty("1.7");
    let widths = object("[550 280 720]");
    let widths = if indirect {
        PdfObject::Reference(doc.add_object(widths))
    } else {
        widths
    };
    let cid = Dictionary::from([
        ("Subtype".into(), PdfObject::Name("CIDFontType2".into())),
        ("DW".into(), PdfObject::Integer(1000)),
        (
            "W".into(),
            PdfObject::Array(vec![PdfObject::Integer(1), widths]),
        ),
    ]);
    let cmap = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        b"3 beginbfchar <0001> <0046> <0002> <0049> <0003> <0044> endbfchar".to_vec(),
    )));
    let font = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Subtype".into(), PdfObject::Name("Type0".into())),
        (
            "BaseFont".into(),
            PdfObject::Name("Synthetic-Regular".into()),
        ),
        ("Encoding".into(), PdfObject::Name("Identity-H".into())),
        ("ToUnicode".into(), PdfObject::Reference(cmap)),
        (
            "DescendantFonts".into(),
            PdfObject::Array(vec![PdfObject::Dictionary(cid)]),
        ),
    ])));
    let content = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        b"BT /F1 20 Tf 10 30 Td [<000100020003>] TJ ET BT /F1 20 Tf 44 30 Td <0003> Tj ET".to_vec(),
    )));
    let pages = ObjectId::new(100, 0);
    let page = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Page".into())),
        ("Parent".into(), PdfObject::Reference(pages)),
        ("MediaBox".into(), object("[0 0 100 60]")),
        ("Contents".into(), PdfObject::Reference(content)),
        (
            "Resources".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "Font".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "F1".into(),
                    PdfObject::Reference(font),
                )])),
            )])),
        ),
    ])));
    doc.set_object(
        pages,
        object(&format!(
            "<< /Type /Pages /Kids [{} 0 R] /Count 1 >>",
            page.number
        )),
    );
    let root = doc.add_object(object("<< /Type /Catalog /Pages 100 0 R >>"));
    doc.set_trailer_key("Root", PdfObject::Reference(root));
    // Exercise the same parse path as an opened document, not just in-memory objects.
    PdfDocument::from_bytes(&doc.to_bytes().unwrap()).unwrap()
}

#[test]
fn indirect_cid_widths_preserve_pixels_and_selection_between_positioned_runs() {
    let direct = document(false);
    let indirect = document(true);
    let direct_page = render_page(&direct, 0, RenderOptions::default()).unwrap();
    let indirect_page = render_page(&indirect, 0, RenderOptions::default()).unwrap();
    assert!(direct_page
        .pixels
        .chunks_exact(4)
        .any(|pixel| pixel[0] < 128));
    assert_eq!(direct_page.pixels, indirect_page.pixels);
    let layout = extract_page_layout(&indirect, 0).unwrap();
    assert_eq!(extract_page_layout(&direct, 0).unwrap(), layout);
    assert_eq!(layout.glyphs.len(), 4);
    for (glyph, x) in layout.glyphs.iter().zip([10.0, 21.0, 26.6, 44.0]) {
        assert!((glyph.bounds[0] - x).abs() < 1e-9, "{:?}", glyph.bounds);
    }
}
