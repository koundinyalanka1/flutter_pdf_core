//! Scanned pages end to end: an image-only PDF is recognized, made
//! searchable, and its text then comes out of the ordinary text APIs.

mod common;

use common::{character_error_rate, scanned, single_spaced, typeset, LINES};
use pdf_core::document::PdfDocument;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_ocr::pdf::{
    apply_ocr, existing_text, make_searchable, ocr_layout, recognize_page, ExternalPage,
};
use pdf_ocr::{OcrEngine, PageStatus, PdfOcrOptions};
use pdf_render::{render_page, RenderOptions};
use pdf_text::extractor::extract_page_text;
use pdf_text::layout::extract_page_layout;

#[test]
fn a_scanned_page_becomes_searchable_and_still_looks_the_same() {
    let engine = OcrEngine::embedded();
    let mut doc = scanned();
    assert_eq!(
        existing_text(&doc, 0).unwrap(),
        None,
        "a scan has no text yet"
    );
    let before = render_page(&doc, 0, RenderOptions::default()).unwrap();

    let reports = make_searchable(&mut doc, &[0], engine, &PdfOcrOptions::default()).unwrap();
    assert_eq!(reports[0].status, PageStatus::Recognized);
    assert!(reports[0].words >= 20, "{reports:?}");

    let doc = PdfDocument::from_bytes(&doc.to_bytes().unwrap()).unwrap();
    let after = render_page(&doc, 0, RenderOptions::default()).unwrap();
    assert!(before.pixels == after.pixels, "the text layer is invisible");
    let text = extract_page_text(&doc, 0).unwrap();
    let error = character_error_rate(&single_spaced(&LINES.join(" ")), &single_spaced(&text));
    assert!(error < 0.03, "CER {:.1}% reading {text:?}", error * 100.0);
    assert_eq!(
        existing_text(&doc, 0).unwrap(),
        Some(PageStatus::HasOcrLayer)
    );
}

#[test]
fn the_instant_layout_matches_the_written_layer() {
    let engine = OcrEngine::embedded();
    let doc = scanned();
    let ocr = recognize_page(&doc, 0, engine, &PdfOcrOptions::default()).unwrap();
    assert!((ocr.dpi - 300.0).abs() < 1.0 && (ocr.width, ocr.height) == (612.0, 792.0));
    let instant = ocr_layout(&doc, &ocr).unwrap();

    let mut written = doc.clone();
    make_searchable(&mut written, &[0], engine, &PdfOcrOptions::default()).unwrap();
    let saved = extract_page_layout(&written, 0).unwrap();
    assert_eq!(instant, saved);
    assert!(!saved.glyphs.is_empty());
    // Word boxes land on the page where the words were typeset: the first
    // line's baseline is 92 points from the top.
    let first = &ocr.lines[0];
    assert!(
        (first.bounds[3] - 92.0).abs() < 6.0 && first.bounds[0] > 70.0 && first.bounds[0] < 76.0,
        "{:?}",
        first.bounds
    );
}

#[test]
fn pages_with_text_are_left_alone_unless_forced() {
    let engine = OcrEngine::embedded();
    let mut doc = typeset();
    let reports = make_searchable(&mut doc, &[0], engine, &PdfOcrOptions::default()).unwrap();
    assert_eq!(reports[0].status, PageStatus::HasText);
    assert_eq!(extract_page_text(&doc, 0).unwrap(), LINES.join("\n"));

    let forced = PdfOcrOptions {
        force: true,
        ..Default::default()
    };
    let reports = make_searchable(&mut doc, &[0], engine, &forced).unwrap();
    assert_eq!(reports[0].status, PageStatus::Recognized);
}

#[test]
fn a_little_text_is_born_digital_unless_a_scan_lies_behind_it() {
    let engine = OcrEngine::embedded();
    // A cover page: a title and nothing else. Recognizing it could only
    // write the title a second time.
    let mut cover = with_content(typeset(), b"BT /F1 24 Tf 72 700 Td (Annual report) Tj ET");
    assert_eq!(existing_text(&cover, 0).unwrap(), Some(PageStatus::HasText));
    let reports = make_searchable(&mut cover, &[0], engine, &PdfOcrOptions::default()).unwrap();
    assert_eq!(reports[0].status, PageStatus::HasText);
    assert_eq!(extract_page_text(&cover, 0).unwrap(), "Annual report");

    // A scan with a page number stamped on afterwards still needs reading.
    let stamped = with_content(
        scanned(),
        b"q 612 0 0 792 0 0 cm /Im0 Do Q BT /F1 9 Tf 540 20 Td (Page 1) Tj ET",
    );
    assert_eq!(existing_text(&stamped, 0).unwrap(), None);
}

/// Replace the page's content, adding Helvetica as /F1 if it has no fonts.
fn with_content(mut doc: PdfDocument, content: &[u8]) -> PdfDocument {
    let page_id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
    let mut resources = page.get("Resources").unwrap().as_dict().unwrap().clone();
    if resources.get("Font").is_none() {
        let font = doc.add_object(PdfObject::Dictionary(Dictionary::from([
            ("Type".into(), PdfObject::Name("Font".into())),
            ("Subtype".into(), PdfObject::Name("Type1".into())),
            ("BaseFont".into(), PdfObject::Name("Helvetica".into())),
        ])));
        resources.insert(
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "F1".into(),
                PdfObject::Reference(font),
            )])),
        );
    }
    let contents = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        content.to_vec(),
    )));
    page.insert("Resources".into(), PdfObject::Dictionary(resources));
    page.insert("Contents".into(), PdfObject::Reference(contents));
    doc.set_object(page_id, PdfObject::Dictionary(page));
    doc
}

#[test]
fn results_from_elsewhere_are_validated_before_they_are_written() {
    let mut doc = scanned();
    let parse = |json: &str| -> Vec<ExternalPage> { serde_json::from_str(json).unwrap() };
    let good = parse(
        r#"[{"page":1,"lines":[{"words":[{"text":"Hello","bounds":[72,80,120,96]},{"text":"there","quad":[[126,80],[170,80],[170,96],[126,96]]}]}]}]"#,
    );
    assert_eq!(apply_ocr(&mut doc, &good).unwrap(), 1);
    assert_eq!(extract_page_text(&doc, 0).unwrap(), "Hello there");

    for bad in [
        r#"[{"page":2,"lines":[]}]"#,
        r#"[{"page":0,"lines":[]}]"#,
        r#"[{"page":1,"lines":[{"words":[{"text":"x"}]}]}]"#,
        r#"[{"page":1,"lines":[{"words":[{"text":"x","bounds":[0,0,1e9,1]}]}]}]"#,
        r#"[{"page":1,"lines":[{"words":[{"text":"a\u0000b","bounds":[0,0,5,5]}]}]}]"#,
    ] {
        assert!(apply_ocr(&mut scanned(), &parse(bad)).is_err(), "{bad}");
    }
}
