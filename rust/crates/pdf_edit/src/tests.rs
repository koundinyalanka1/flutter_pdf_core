use std::collections::BTreeMap;

use pdf_core::document::PdfDocument;
use pdf_core::error::PdfError;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;

use crate::content::{decode_text, dict, name};
use crate::*;

fn r(n: u32) -> PdfObject {
    PdfObject::Reference(ObjectId::new(n, 0))
}

fn s(text: &str) -> PdfObject {
    PdfObject::LiteralString(text.as_bytes().to_vec())
}

fn int(v: i64) -> PdfObject {
    PdfObject::Integer(v)
}

fn nums(v: &[f64]) -> PdfObject {
    crate::content::numbers(v)
}

fn d(entries: Vec<(&str, PdfObject)>) -> PdfObject {
    PdfObject::Dictionary(dict(entries))
}

fn stream(text: &str) -> PdfObject {
    let bbox = dict(vec![("Type", name("XObject")), ("Subtype", name("Form")), ("BBox", nums(&[0.0, 0.0, 10.0, 10.0]))]);
    PdfObject::Stream(PdfStream::new(bbox, text.as_bytes().to_vec()))
}

fn widget(rect: [f64; 4], page: u32, extra: Vec<(&str, PdfObject)>) -> PdfObject {
    let mut entries = vec![
        ("Type", name("Annot")),
        ("Subtype", name("Widget")),
        ("Rect", nums(&rect)),
        ("P", r(page)),
        ("F", int(4)),
    ];
    entries.extend(extra);
    d(entries)
}

/// A two-page form with a field of every kind; the second page is turned
/// a quarter.
fn form_pdf() -> Vec<u8> {
    let mut doc = PdfDocument::new_empty("1.7");
    let mut set = |n: u32, v: PdfObject| doc.set_object(ObjectId::new(n, 0), v);
    set(1, d(vec![("Type", name("Catalog")), ("Pages", r(2)), ("AcroForm", r(5))]));
    set(2, d(vec![("Type", name("Pages")), ("Kids", PdfObject::Array(vec![r(3), r(4)])), ("Count", int(2))]));
    let annots: Vec<PdfObject> = [20, 21, 23, 24, 25, 26, 28, 29, 32, 33].iter().map(|n| r(*n)).collect();
    set(
        3,
        d(vec![
            ("Type", name("Page")),
            ("Parent", r(2)),
            ("MediaBox", nums(&[0.0, 0.0, 612.0, 792.0])),
            ("Contents", r(10)),
            ("Annots", PdfObject::Array(annots)),
            ("Resources", d(vec![])),
        ]),
    );
    set(
        4,
        d(vec![
            ("Type", name("Page")),
            ("Parent", r(2)),
            ("MediaBox", nums(&[0.0, 0.0, 612.0, 792.0])),
            ("Rotate", int(90)),
            ("Contents", r(11)),
        ]),
    );
    set(
        5,
        d(vec![
            ("Fields", PdfObject::Array([20, 21, 22, 25, 26, 27, 32, 33].iter().map(|n| r(*n)).collect())),
            ("DR", d(vec![("Font", d(vec![("Helv", r(6))]))])),
            ("DA", s("/Helv 0 Tf 0 g")),
            ("XFA", PdfObject::Array(vec![s("template"), r(12)])),
        ]),
    );
    set(6, d(vec![("Type", name("Font")), ("Subtype", name("Type1")), ("BaseFont", name("Helvetica")), ("Encoding", name("WinAnsiEncoding"))]));
    set(10, PdfObject::Stream(PdfStream::new(Dictionary::new(), b"BT /F1 12 Tf 72 720 Td (Hello) Tj ET".to_vec())));
    set(11, PdfObject::Stream(PdfStream::new(Dictionary::new(), b"0 0 1 rg 10 10 50 50 re f".to_vec())));
    set(12, PdfObject::Stream(PdfStream::new(Dictionary::new(), b"<template/>".to_vec())));
    set(20, widget([50.0, 700.0, 300.0, 720.0], 3, vec![("FT", name("Tx")), ("T", s("name")), ("DA", s("/Helv 12 Tf 0 0 1 rg"))]));
    let states = |on: &str| d(vec![("N", d(vec![(on, r(30)), ("Off", r(31))]))]);
    set(21, widget([50.0, 650.0, 62.0, 662.0], 3, vec![("FT", name("Btn")), ("T", s("agree")), ("AP", states("Yes")), ("AS", name("Off")), ("V", name("Off"))]));
    set(22, d(vec![("FT", name("Btn")), ("Ff", int(1 << 15)), ("T", s("size")), ("Kids", PdfObject::Array(vec![r(23), r(24)])), ("V", name("Off"))]));
    set(23, widget([50.0, 600.0, 62.0, 612.0], 3, vec![("Parent", r(22)), ("AP", states("S")), ("AS", name("Off"))]));
    set(24, widget([80.0, 600.0, 92.0, 612.0], 3, vec![("Parent", r(22)), ("AP", states("M")), ("AS", name("Off"))]));
    set(
        25,
        widget(
            [50.0, 550.0, 200.0, 570.0],
            3,
            vec![
                ("FT", name("Ch")),
                ("Ff", int(1 << 17)),
                ("T", s("color")),
                ("Opt", PdfObject::Array(vec![s("Red"), s("Green"), PdfObject::Array(vec![s("b"), s("Blue")])])),
            ],
        ),
    );
    set(
        26,
        widget(
            [50.0, 450.0, 200.0, 520.0],
            3,
            vec![
                ("FT", name("Ch")),
                ("Ff", int(1 << 21)),
                ("T", s("fruits")),
                ("Opt", PdfObject::Array(vec![s("Apple"), s("Banana"), s("Cherry")])),
                ("MK", d(vec![("BC", nums(&[0.0])), ("BG", nums(&[1.0, 1.0, 0.9]))])),
            ],
        ),
    );
    set(27, d(vec![("T", s("person")), ("FT", name("Tx")), ("Kids", PdfObject::Array(vec![r(28), r(29)]))]));
    set(28, widget([50.0, 400.0, 200.0, 420.0], 3, vec![("Parent", r(27)), ("T", s("first"))]));
    set(29, widget([250.0, 400.0, 400.0, 420.0], 3, vec![("Parent", r(27)), ("T", s("last")), ("MaxLen", int(4)), ("Ff", int(1 << 24))]));
    set(30, stream("0 g 0 0 10 10 re f"));
    set(31, stream(""));
    set(32, widget([50.0, 350.0, 200.0, 370.0], 3, vec![("FT", name("Tx")), ("T", s("id")), ("Ff", int(1)), ("V", s("A1"))]));
    set(33, widget([50.0, 300.0, 62.0, 312.0], 3, vec![("FT", name("Btn")), ("T", s("plain")), ("MK", d(vec![("CA", s("8"))]))]));
    doc.set_trailer_key("Root", r(1));
    doc.to_bytes().unwrap()
}

fn field<'a>(info: &'a FormInfo, name: &str) -> &'a FieldInfo {
    info.fields.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no field {name}"))
}

fn edit(name: &str, value: Option<&str>, checked: Option<bool>, selected: Option<&[&str]>) -> FieldEdit {
    FieldEdit {
        name: name.into(),
        value: value.map(str::to_owned),
        checked,
        selected: selected.map(|s| s.iter().map(|v| (*v).to_owned()).collect()),
    }
}

fn dict_of(doc: &PdfDocument, n: u32) -> Dictionary {
    doc.resolve(ObjectId::new(n, 0)).and_then(PdfObject::as_dict).cloned().unwrap()
}

fn appearance_text(doc: &PdfDocument, widget: u32) -> String {
    let w = dict_of(doc, widget);
    let ap = doc.resolve_dict(w.get("AP").unwrap()).unwrap();
    let normal = match doc.resolve_value(ap.get("N").unwrap()) {
        // A button's appearances by state: the one it is in.
        PdfObject::Dictionary(states) => {
            let state = w.get("AS").and_then(PdfObject::as_name).unwrap_or("Off").to_owned();
            doc.resolve_value(states.get(&state).unwrap())
        }
        other => other,
    };
    let PdfObject::Stream(stream) = normal else { panic!("no appearance") };
    String::from_utf8_lossy(&doc.stream_data(&stream).unwrap()).into_owned()
}

#[test]
fn lists_every_kind_of_field() {
    let doc = PdfDocument::from_bytes(&form_pdf()).unwrap();
    let info = fields(&doc);
    assert!(info.xfa);
    assert_eq!(
        info.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["name", "agree", "size", "color", "fruits", "person.first", "person.last", "id", "plain"]
    );
    assert_eq!(field(&info, "agree").kind, FieldKind::Checkbox);
    assert_eq!(field(&info, "agree").states, ["Yes"]);
    let size = field(&info, "size");
    assert_eq!((size.kind, size.states.clone(), size.value.as_str()), (FieldKind::Radio, vec!["S".into(), "M".into()], "Off"));
    assert_eq!(size.widgets.len(), 2);
    let color = field(&info, "color");
    assert!(color.combo && color.kind == FieldKind::Choice);
    assert_eq!((color.options[2].value.as_str(), color.options[2].label.as_str()), ("b", "Blue"));
    assert!(field(&info, "fruits").multi_select);
    assert!(field(&info, "person.last").comb);
    assert!(field(&info, "id").read_only);
    assert_eq!(field(&info, "id").value, "A1");
    // Displayed rectangles have a top-left origin.
    let w = &field(&info, "name").widgets[0];
    assert_eq!((w.page, w.rect, w.font_size), (1, [50.0, 72.0, 300.0, 92.0], 12.0));
}

#[test]
fn fills_every_kind_of_field_in_an_update() {
    let original = form_pdf();
    let edits = Edits {
        fields: vec![
            edit("name", Some("Zoë Ångström"), None, None),
            edit("agree", None, Some(true), None),
            edit("size", Some("M"), None, None),
            edit("color", Some("b"), None, None),
            edit("fruits", None, None, Some(&["Apple", "Cherry"])),
            edit("person.first", Some("Ada"), None, None),
            edit("person.last", Some("Lovelace"), None, None),
            edit("id", Some("B2"), None, None),
            edit("plain", None, Some(true), None),
        ],
        ..Default::default()
    };
    let (bytes, warnings) = apply(original.clone(), "", &edits).unwrap();
    assert!(bytes.starts_with(&original), "the original bytes stay as they were");
    assert_eq!(warnings, ["\"id\" is read-only and was left as it was."]);
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    let info = fields(&doc);
    assert!(!info.xfa, "XFA goes so readers show these values");
    assert_eq!(field(&info, "name").value, "Zoë Ångström");
    assert_eq!(field(&info, "agree").value, "Yes");
    assert_eq!(dict_of(&doc, 21).get("AS"), Some(&name("Yes")));
    assert_eq!(field(&info, "size").value, "M");
    assert_eq!(dict_of(&doc, 23).get("AS"), Some(&name("Off")));
    assert_eq!(dict_of(&doc, 24).get("AS"), Some(&name("M")));
    assert_eq!(field(&info, "color").value, "b");
    assert_eq!(field(&info, "fruits").values, ["Apple", "Cherry"]);
    assert_eq!(dict_of(&doc, 26).get("I"), Some(&PdfObject::Array(vec![int(0), int(2)])));
    assert_eq!(field(&info, "person.first").value, "Ada");
    assert_eq!(field(&info, "person.last").value, "Love", "MaxLen cuts the value");
    assert_eq!(field(&info, "id").value, "A1");
    assert_eq!(field(&info, "plain").value, "Yes", "a check box without appearances gets them");

    let name_ap = appearance_text(&doc, 20);
    assert!(name_ap.contains("/Tx BMC") && name_ap.contains("0 0 1 rg"), "{name_ap}");
    assert!(name_ap.contains("(Zo\\353 \\305ngstr\\366m) Tj"), "WinAnsi text: {name_ap}");
    assert!(appearance_text(&doc, 25).contains("(Blue) Tj"), "a drop-down shows the label");
    let list = appearance_text(&doc, 26);
    assert!(list.contains("0.6 0.75 0.85 rg") && list.contains("(Cherry) Tj") && list.contains("1 1 0.9 rg"), "{list}");
    // A comb field puts each character in its own cell.
    assert_eq!(appearance_text(&doc, 29).matches(" Tj").count(), 4);
    assert!(appearance_text(&doc, 33).contains("(8) Tj"), "the check box's own glyph");
}

#[test]
fn unchecking_and_clearing() {
    let (bytes, _) = apply(form_pdf(), "", &Edits { fields: vec![edit("agree", None, Some(true), None)], ..Default::default() }).unwrap();
    let (bytes, _) = apply(bytes, "", &Edits { fields: vec![edit("agree", None, Some(false), None), edit("size", Some("Off"), None, None)], ..Default::default() }).unwrap();
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    let info = fields(&doc);
    assert_eq!(field(&info, "agree").value, "Off");
    assert_eq!(dict_of(&doc, 21).get("AS"), Some(&name("Off")));
    assert_eq!(field(&info, "size").value, "Off");
}

fn annotation(page: u32, kind: Kind) -> Annotation {
    Annotation {
        page,
        kind,
        color: Color(1.0, 0.9, 0.2),
        fill: None,
        width: 2.0,
        opacity: 1.0,
        contents: String::new(),
        author: "Tester".into(),
        flatten: false,
    }
}

#[test]
fn annotations_land_where_they_were_drawn_and_can_be_removed() {
    let mut edits = Edits::default();
    for page in [1, 2] {
        edits.annotations.push(annotation(page, Kind::Highlight { rects: vec![[100.0, 100.0, 200.0, 120.0]] }));
        edits.annotations.push(annotation(page, Kind::Note { at: [300.0, 50.0], text: "Look here ✓".into() }));
        edits.annotations.push(annotation(page, Kind::Ink { strokes: vec![vec![[10.0, 10.0], [20.0, 30.0], [40.0, 15.0]]] }));
    }
    let (bytes, warnings) = apply(form_pdf(), "", &edits).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    for page in [0, 1] {
        let list = annotations(&doc, page).unwrap();
        let ours: Vec<&AnnotInfo> = list.iter().filter(|a| a.ours).collect();
        assert_eq!(ours.iter().map(|a| a.subtype.as_str()).collect::<Vec<_>>(), ["Highlight", "Text", "Ink"]);
        let h = ours[0].rect;
        assert!((h[0] - 98.0).abs() < 0.5 && (h[1] - 98.0).abs() < 0.5 && (h[2] - 202.0).abs() < 0.5 && (h[3] - 122.0).abs() < 0.5, "page {page}: {h:?}");
        assert_eq!(ours[1].rect, [300.0, 50.0, 324.0, 74.0], "page {page}");
        assert_eq!(ours[1].contents, "Look here ✓");
        assert_eq!(ours[1].author, "Tester");
    }
    // On the turned page, the highlight's quadrilateral starts at the
    // text's top-left as shown.
    let pages = doc.collect_page_ids().unwrap();
    let page2 = doc.resolve(pages[1]).unwrap().as_dict().unwrap();
    let PdfObject::Array(annots) = doc.resolve_value(page2.get("Annots").unwrap()) else { panic!() };
    let highlight = doc.resolve(annots[0].as_ref().unwrap()).unwrap().as_dict().unwrap();
    let PdfObject::Array(quad) = highlight.get("QuadPoints").unwrap() else { panic!() };
    let q: Vec<f64> = quad.iter().map(|v| match v { PdfObject::Integer(i) => *i as f64, PdfObject::Real(r) => *r, _ => 0.0 }).collect();
    // Displayed (100, 100) on a page turned 90°: user x = 100, y = 100.
    assert_eq!(&q[..2], &[100.0, 100.0]);

    let note = annotations(&doc, 1).unwrap().into_iter().find(|a| a.subtype == "Text").unwrap();
    let edits = Edits { delete: vec![Delete { page: 2, id: note.id.clone() }], ..Default::default() };
    let (bytes, _) = apply(bytes, "", &edits).unwrap();
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(annotations(&doc, 1).unwrap().iter().all(|a| a.id != note.id));
    assert_eq!(annotations(&doc, 0).unwrap().iter().filter(|a| a.ours).count(), 3);
}

#[test]
fn every_kind_of_annotation_has_an_appearance() {
    let dir = tempfile::tempdir().unwrap();
    let rgba = dir.path().join("dot.rgba");
    std::fs::write(&rgba, [255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 128, 255, 255, 255, 255]).unwrap();
    let kinds = vec![
        Kind::Underline { rects: vec![[10.0, 10.0, 90.0, 22.0]] },
        Kind::StrikeOut { rects: vec![[10.0, 30.0, 90.0, 42.0]] },
        Kind::Squiggly { rects: vec![[10.0, 50.0, 90.0, 62.0]] },
        Kind::Rectangle { rect: [10.0, 70.0, 90.0, 100.0] },
        Kind::Ellipse { rect: [10.0, 110.0, 90.0, 140.0] },
        Kind::Line { from: [10.0, 150.0], to: [90.0, 160.0] },
        Kind::Arrow { from: [10.0, 170.0], to: [90.0, 180.0] },
        Kind::FreeText { rect: [10.0, 190.0, 200.0, 240.0], text: "Hello there, this wraps".into(), font_size: 14.0 },
        Kind::Text { rect: [10.0, 250.0, 200.0, 270.0], text: "Typed".into(), font_size: 12.0 },
        Kind::Signature { rect: [10.0, 280.0, 120.0, 320.0], strokes: vec![vec![[15.0, 300.0], [40.0, 285.0], [80.0, 315.0]]] },
        Kind::Image { rect: [130.0, 280.0, 170.0, 320.0], image: ImageSource { path: rgba.to_string_lossy().into(), width: 2, height: 2 } },
    ];
    let edits = Edits { annotations: kinds.into_iter().map(|k| annotation(1, k)).collect(), ..Default::default() };
    let (bytes, _) = apply(form_pdf(), "", &edits).unwrap();
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    let ours: Vec<AnnotInfo> = annotations(&doc, 0).unwrap().into_iter().filter(|a| a.ours).collect();
    assert_eq!(
        ours.iter().map(|a| a.subtype.as_str()).collect::<Vec<_>>(),
        ["Underline", "StrikeOut", "Squiggly", "Square", "Circle", "Line", "Line", "FreeText", "FreeText", "Stamp", "Stamp"]
    );
    for a in &ours {
        let n: u32 = a.id.split(' ').next().unwrap().parse().unwrap();
        let text = appearance_text(&doc, n);
        assert!(!text.trim().is_empty(), "{} has an empty appearance", a.subtype);
    }
    let free = ours.iter().find(|a| a.subtype == "FreeText").unwrap();
    assert_eq!(free.contents, "Hello there, this wraps");
    // The picture's transparency is a soft mask.
    let stamp: u32 = ours.last().unwrap().id.split(' ').next().unwrap().parse().unwrap();
    let ap = doc.resolve_dict(dict_of(&doc, stamp).get("AP").unwrap()).unwrap().clone();
    let PdfObject::Stream(form) = doc.resolve_value(ap.get("N").unwrap()) else { panic!() };
    let res = doc.resolve_dict(form.dictionary.get("Resources").unwrap()).unwrap();
    let xo = doc.resolve_dict(res.get("XObject").unwrap()).unwrap();
    let PdfObject::Stream(image) = doc.resolve_value(xo.get("Im0").unwrap()) else { panic!() };
    assert!(image.dictionary.contains_key("SMask"));
}

#[test]
fn flattening_draws_into_the_page() {
    let mut a = annotation(1, Kind::Text { rect: [72.0, 100.0, 300.0, 120.0], text: "Signed here".into(), font_size: 12.0 });
    a.flatten = true;
    let edits = Edits { annotations: vec![a], ..Default::default() };
    let (bytes, _) = apply(form_pdf(), "", &edits).unwrap();
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(annotations(&doc, 0).unwrap().iter().all(|a| !a.ours), "no annotation is left");
    let page = dict_of(&doc, 3);
    let PdfObject::Array(contents) = page.get("Contents").unwrap() else { panic!("contents became an array") };
    assert_eq!(contents.len(), 3);
    let text = pdf_text::layout::extract_page_layout(&doc, 0).unwrap();
    assert!(text.text.contains("Hello") && text.text.contains("Signed here"), "{}", text.text);
    let glyph = text.glyphs.iter().find(|g| g.bounds[0] > 70.0 && g.bounds[1] > 95.0 && g.bounds[3] < 125.0);
    assert!(glyph.is_some(), "the text is where it was typed");
}

#[test]
fn marks_stand_upright_on_turned_pages() {
    let mark = |position, pages: Vec<u32>| Mark {
        pages,
        text: "Page {page} of {pages}".into(),
        image: None,
        size: None,
        position,
        font_size: 10.0,
        font: FontChoice::Sans,
        bold: false,
        color: Color::BLACK,
        opacity: 1.0,
        rotation: 0.0,
        margin: 20.0,
    };
    let edits = Edits { marks: vec![mark(Position::BottomCenter, vec![]), mark(Position::TopRight, vec![2])], ..Default::default() };
    let (bytes, _) = apply(form_pdf(), "", &edits).unwrap();
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    for (index, (w, h)) in [(612.0, 792.0), (792.0, 612.0)].into_iter().enumerate() {
        let layout = pdf_text::layout::extract_page_layout(&doc, index).unwrap();
        assert_eq!((layout.width, layout.height), (w, h));
        let label = format!("Page {} of 2", index + 1);
        let at = layout.text.find(&label).unwrap_or_else(|| panic!("{label} in {:?}", layout.text));
        let start = layout.text[..at].encode_utf16().count();
        let glyphs: Vec<_> = layout.glyphs.iter().filter(|g| g.start >= start && g.start < start + label.len()).collect();
        let left = glyphs.iter().map(|g| g.bounds[0]).fold(f64::INFINITY, f64::min);
        let right = glyphs.iter().map(|g| g.bounds[2]).fold(0.0, f64::max);
        let bottom = glyphs.iter().map(|g| g.bounds[3]).fold(0.0, f64::max);
        assert!(((left + right) / 2.0 - w / 2.0).abs() < 2.0, "centred on page {}: {left}..{right}", index + 1);
        assert!(bottom < h - 15.0 && bottom > h - 30.0, "near the bottom of page {}: {bottom}", index + 1);
        // Upright: glyphs run left to right along one line.
        assert!(glyphs.windows(2).all(|p| p[1].bounds[0] >= p[0].bounds[0] - 0.01));
    }
    let layout = pdf_text::layout::extract_page_layout(&doc, 1).unwrap();
    assert_eq!(layout.text.matches("Page 2 of 2").count(), 2);
}

#[test]
fn encrypted_files_keep_their_password() {
    let fixtures = [
        (&include_bytes!("../../../fixtures/rc4_128_latin1_password.pdf")[..], "Pässwörd1"),
        (&include_bytes!("../../../fixtures/aes128_latin1_password.pdf")[..], "Pässwörd1"),
        (&include_bytes!("../../../fixtures/aes256_saslprep_password.pdf")[..], "Päss wörd1"),
    ];
    for (bytes, password) in fixtures {
        let edits = Edits {
            annotations: vec![annotation(1, Kind::Note { at: [10.0, 10.0], text: "Geheim — ✓".into() })],
            ..Default::default()
        };
        let (out, _) = apply(bytes.to_vec(), password, &edits).unwrap();
        assert!(out.starts_with(bytes));
        assert!(matches!(PdfDocument::from_bytes(&out), Err(PdfError::Encrypted | PdfError::WrongPassword)));
        let doc = PdfDocument::from_bytes_with_password(&out, password).unwrap();
        let notes = annotations(&doc, 0).unwrap();
        let note = notes.iter().find(|a| a.ours).expect("the note");
        assert_eq!(note.contents, "Geheim — ✓");
        // Its appearance decrypts to drawing operators.
        let n: u32 = note.id.split(' ').next().unwrap().parse().unwrap();
        assert!(appearance_text(&doc, n).contains(" cm"));
    }
}

/// The form written with a cross-reference stream instead of a table.
fn with_xref_stream(classic: &[u8]) -> Vec<u8> {
    let doc = PdfDocument::from_bytes(classic).unwrap();
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = BTreeMap::new();
    for (id, obj) in &doc.objects {
        offsets.insert(id.number, out.len());
        out.extend_from_slice(format!("{} 0 obj\n", id.number).as_bytes());
        out.extend_from_slice(&pdf_core::writer::PdfWriter::serialize_object(&obj.value).unwrap());
        out.extend_from_slice(b"\nendobj\n");
    }
    let own = doc.max_object_number() + 1;
    let at = out.len();
    offsets.insert(own, at);
    let mut data = vec![0u8, 0, 0, 0, 0, 0xFF, 0xFF];
    for n in 1..=own {
        match offsets.get(&n) {
            Some(off) => {
                data.push(1);
                data.extend_from_slice(&(*off as u32).to_be_bytes());
                data.extend_from_slice(&[0, 0]);
            }
            None => data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]),
        }
    }
    let xref = dict(vec![
        ("Type", name("XRef")),
        ("Size", int(own as i64 + 1)),
        ("W", PdfObject::Array(vec![int(1), int(4), int(2)])),
        ("Root", doc.xref.trailer.get("Root").unwrap().clone()),
    ]);
    out.extend_from_slice(format!("{own} 0 obj\n").as_bytes());
    out.extend_from_slice(&pdf_core::writer::PdfWriter::serialize_object(&PdfObject::Stream(PdfStream::new(xref, data))).unwrap());
    out.extend_from_slice(format!("\nendobj\nstartxref\n{at}\n%%EOF\n").as_bytes());
    out
}

#[test]
fn a_file_with_an_xref_stream_gets_one_in_its_update() {
    let original = with_xref_stream(&form_pdf());
    assert!(PdfDocument::from_bytes(&original).unwrap().recovery_warnings.is_empty());
    let edits = Edits { fields: vec![edit("name", Some("Stream"), None, None)], ..Default::default() };
    let (bytes, _) = apply(original.clone(), "", &edits).unwrap();
    assert!(bytes.starts_with(&original));
    let update = &bytes[original.len()..];
    assert!(!update.windows(6).any(|w| w == b"\nxref\n"), "no table in the update");
    assert!(update.windows(10).any(|w| w == b"/Type/XRef") || update.windows(11).any(|w| w == b"/Type /XRef"));
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    assert_eq!(field(&fields(&doc), "name").value, "Stream");
    // A second update chains to the first.
    let (again, _) = apply(bytes.clone(), "", &Edits { fields: vec![edit("name", Some("Twice"), None, None)], ..Default::default() }).unwrap();
    let doc = PdfDocument::from_bytes(&again).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    assert_eq!(field(&fields(&doc), "name").value, "Twice");
}

#[test]
fn a_damaged_file_is_written_whole() {
    let mut original = form_pdf();
    // Point startxref at nothing.
    let at = original.windows(9).rposition(|w| w == b"startxref").unwrap();
    original.truncate(at);
    original.extend_from_slice(b"startxref\n999999\n%%EOF\n");
    let edits = Edits { fields: vec![edit("name", Some("Mended"), None, None)], ..Default::default() };
    let (bytes, _) = apply(original.clone(), "", &edits).unwrap();
    assert!(!bytes.starts_with(&original));
    let doc = PdfDocument::from_bytes(&bytes).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    assert_eq!(field(&fields(&doc), "name").value, "Mended");
}

#[test]
fn nothing_to_do_leaves_the_file_as_it_was() {
    let original = form_pdf();
    let (bytes, _) = apply(original.clone(), "", &Edits::default()).unwrap();
    assert_eq!(bytes, original);
}

#[test]
fn edits_read_from_json() {
    let json = r##"{
        "fields": [{"name": "name", "value": "Json"}],
        "annotations": [
            {"page": 1, "type": "highlight", "rects": [[1, 2, 3, 4]], "color": "#FFFF00", "opacity": 0.5},
            {"page": 1, "type": "freeText", "rect": [1, 2, 30, 40], "text": "hi", "fontSize": 9},
            {"page": 1, "type": "image", "rect": [1, 2, 3, 4], "image": {"path": "/x.jpg"}, "flatten": true}
        ],
        "delete": [{"page": 1, "id": "12 0"}],
        "marks": [{"text": "{page}", "position": "bottomRight", "color": "#FF336699"}]
    }"##;
    let edits: Edits = serde_json::from_str(json).unwrap();
    assert_eq!(edits.annotations.len(), 3);
    assert!(matches!(edits.annotations[1].kind, Kind::FreeText { font_size, .. } if font_size == 9.0));
    assert_eq!(edits.annotations[0].color, Color(1.0, 1.0, 0.0));
    assert_eq!(edits.marks[0].color.hex(), "#336699");
    assert!(edits.annotations[2].flatten);
    let decoded = decode_text(&[0xFE, 0xFF, 0, b'h', 0, b'i']);
    assert_eq!(decoded, "hi");
}
