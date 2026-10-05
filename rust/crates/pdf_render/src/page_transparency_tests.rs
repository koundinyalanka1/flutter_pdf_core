//! Independent expected colours and geometry for PDF graphics-state semantics.
use super::tests::{doc_with_content, pixel};
use super::*;
use pdf_core::{parser::Parser, stream::PdfStream};

fn object(s: &str) -> PdfObject {
    Parser::new(s.as_bytes()).parse_object().unwrap()
}
fn resources(doc: &mut PdfDocument, s: &str) {
    let id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
    page.insert("Resources".into(), object(s));
    doc.set_object(id, PdfObject::Dictionary(page));
}
fn form(doc: &mut PdfDocument, dict: &str, content: &str) -> u32 {
    doc.add_object(PdfObject::Stream(PdfStream::new(
        object(dict).as_dict().unwrap().clone(),
        content.as_bytes().to_vec(),
    )))
    .number
}
fn render(doc: &PdfDocument) -> RenderedPage {
    render_page(doc, 0, RenderOptions::default()).unwrap()
}
fn close(actual: (u8, u8, u8), expected: (u8, u8, u8)) {
    for (a, b) in [actual.0, actual.1, actual.2]
        .into_iter()
        .zip([expected.0, expected.1, expected.2])
    {
        assert!(
            a.abs_diff(b) <= 2,
            "actual {actual:?}, expected {expected:?}"
        );
    }
}

#[test]
fn group_opacity_is_applied_once_to_overlapping_objects() {
    let mut doc = doc_with_content("/Half gs /G Do", [0, 0, 60, 40]);
    let g = form(
        &mut doc,
        "<< /Subtype /Form /BBox [0 0 60 40] /Group << /S /Transparency /I true >> >>",
        "1 0 0 rg 0 0 40 40 re f 0 0 1 rg 20 0 40 40 re f",
    );
    resources(
        &mut doc,
        &format!("<< /ExtGState << /Half << /ca 0.5 >> >> /XObject << /G {g} 0 R >> >>"),
    );
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    close(pixel(&page, 10, 20), (255, 128, 128));
    close(pixel(&page, 30, 20), (128, 128, 255));
    close(pixel(&page, 50, 20), (128, 128, 255));
}

#[test]
fn isolated_and_nonisolated_groups_have_different_backdrops() {
    for (isolated, expected) in [(true, (0, 0, 255)), (false, (0, 0, 0))] {
        let mut doc = doc_with_content("1 0 0 rg 0 0 40 40 re f /G Do", [0, 0, 40, 40]);
        let g = form(&mut doc, &format!("<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency /I {isolated} >> /Resources << /ExtGState << /M << /BM /Multiply >> >> >> >>"), "/M gs 0 0 1 rg 0 0 40 40 re f");
        resources(&mut doc, &format!("<< /XObject << /G {g} 0 R >> >>"));
        close(pixel(&render(&doc), 20, 20), expected);
    }
}

#[test]
fn knockout_and_alpha_is_shape_control_overlap() {
    for (ais, expected) in [(false, (128, 128, 255)), (true, (128, 64, 191))] {
        let mut doc = doc_with_content("/G Do", [0, 0, 40, 40]);
        let g = form(&mut doc, &format!("<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency /I true /K true >> /Resources << /ExtGState << /H << /ca 0.5 /AIS {ais} >> >> >> >>"), "/H gs 1 0 0 rg 0 0 40 40 re f 0 0 1 rg 0 0 40 40 re f");
        resources(&mut doc, &format!("<< /XObject << /G {g} 0 R >> >>"));
        close(pixel(&render(&doc), 20, 20), expected);
    }
}

#[test]
fn alpha_and_luminosity_soft_masks_transform_transfer_and_restore() {
    for (kind, content, expected) in [
        ("Alpha", "/H gs 1 g 0 0 20 40 re f", 128),
        ("Luminosity", "0.25 g 0 0 20 40 re f", 64),
    ] {
        let mut doc = doc_with_content(
            "q /M gs 0 g 0 0 40 40 re f Q 1 0 0 rg 35 0 5 40 re f",
            [0, 0, 40, 40],
        );
        let g = form(&mut doc, "<< /Subtype /Form /BBox [0 0 20 40] /Matrix [1 0 0 1 10 0] /Group << /S /Transparency /I true >> /Resources << /ExtGState << /H << /ca 0.5 >> >> >> >>", content);
        resources(&mut doc, &format!("<< /ExtGState << /M << /SMask << /S /{kind} /G {g} 0 R /TR << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >> >> >> >> >>"));
        let page = render(&doc);
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        close(pixel(&page, 20, 20), (expected, expected, expected));
        assert_eq!(pixel(&page, 5, 20), (0, 0, 0)); // zero mask inverted by TR
        assert_eq!(pixel(&page, 37, 20), (255, 0, 0)); // Q restores unmasked painting
    }
}

#[test]
fn blend_array_selects_first_supported_mode_and_q_restores_it() {
    let mut doc = doc_with_content(
        "1 0 0 rg 0 0 40 40 re f q /M gs 0 0 1 rg 0 0 20 40 re f Q 0 0 1 rg 20 0 20 40 re f",
        [0, 0, 40, 40],
    );
    resources(
        &mut doc,
        "<< /ExtGState << /M << /BM [/Unknown /Multiply /Screen] >> >> >>",
    );
    let page = render(&doc);
    assert!(page.warnings.is_empty());
    assert_eq!(pixel(&page, 10, 20), (0, 0, 0));
    assert_eq!(pixel(&page, 30, 20), (0, 0, 255));
}

#[test]
fn miter_limit_and_bevel_have_distinct_corner_geometry() {
    let render_style = |join, limit| {
        render(&doc_with_content(
            &format!("10 w {join} j {limit} M 10 10 m 30 10 l 30 30 l S"),
            [0, 0, 40, 40],
        ))
    };
    assert_eq!(pixel(&render_style(0, 10), 34, 34), (0, 0, 0));
    assert_eq!(pixel(&render_style(2, 10), 34, 34), (255, 255, 255));
    assert_eq!(pixel(&render_style(0, 1), 34, 34), (255, 255, 255));
    let round = render_style(1, 10);
    assert!(pixel(&round, 32, 32).0 < 10);
}

fn text_doc(content: &str) -> PdfDocument {
    let mut doc = doc_with_content(content, [0, 0, 120, 70]);
    resources(
        &mut doc,
        "<< /Font << /F << /Subtype /Type1 /BaseFont /Helvetica >> >> >>",
    );
    doc
}
#[test]
fn text_clip_accumulates_glyphs_until_et_and_ends_at_q() {
    let text = "BT /F 40 Tf 7 Tr 10 10 Td (H) Tj 40 0 Td (H) Tj ET";
    let page = render(&text_doc(&format!(
        "q {text} 1 0 0 rg 0 0 120 70 re f Q 0 0 1 rg 110 0 10 70 re f"
    )));
    let red = |range: std::ops::Range<usize>| {
        range
            .flat_map(|x| (0..70).map(move |y| (x, y)))
            .filter(|&(x, y)| pixel(&page, x as u32, y) == (255, 0, 0))
            .count()
    };
    assert!(red(10..40) > 100);
    assert!(red(50..80) > 100);
    assert_eq!(pixel(&page, 100, 35), (255, 255, 255));
    assert_eq!(pixel(&page, 115, 35), (0, 0, 255));
}
#[test]
fn stroked_text_is_hollow_and_fill_stroke_uses_both_colours() {
    let page = |mode| {
        render(&text_doc(&format!(
            "1 0 0 rg 0 0 1 RG 1 w BT /F 50 Tf {mode} Tr 10 10 Td (H) Tj ET"
        )))
    };
    let stroke = page(1);
    assert!(!stroke
        .pixels
        .chunks_exact(4)
        .any(|p| p[0] > 240 && p[1] < 10 && p[2] < 10));
    let both = page(2);
    let count = |color: [u8; 3]| {
        both.pixels
            .chunks_exact(4)
            .filter(|p| p[..3] == color)
            .count()
    };
    assert!(count([255, 0, 0]) > 100);
    assert!(count([0, 0, 255]) > 20);
}

#[test]
fn nonisolated_child_of_knockout_group_inherits_initial_backdrop() {
    let mut doc = doc_with_content("/Outer Do", [0, 0, 40, 40]);
    let child = form(&mut doc, "<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency >> /Resources << /ExtGState << /M << /BM /Multiply >> >> >> >>", "/M gs 0 0 1 rg 0 0 40 40 re f");
    let parent = form(&mut doc, &format!("<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency /I true /K true >> /Resources << /XObject << /Child {child} 0 R >> >> >>"), "1 0 0 rg 0 0 40 40 re f /Child Do");
    resources(
        &mut doc,
        &format!("<< /XObject << /Outer {parent} 0 R >> >>"),
    );
    assert_eq!(pixel(&render(&doc), 20, 20), (0, 0, 255));
}

#[test]
fn explicit_group_colour_space_implies_isolation() {
    let mut doc = doc_with_content("1 0 0 rg 0 0 40 40 re f /G Do", [0, 0, 40, 40]);
    let g = form(&mut doc, "<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency /CS /DeviceRGB >> /Resources << /ExtGState << /M << /BM /Multiply >> >> >> >>", "/M gs 0 0 1 rg 0 0 40 40 re f");
    resources(&mut doc, &format!("<< /XObject << /G {g} 0 R >> >>"));
    assert_eq!(pixel(&render(&doc), 20, 20), (0, 0, 255));
}

#[test]
fn indirect_alpha_and_extgstate_font_are_resolved() {
    let mut doc = doc_with_content("/G gs BT 10 10 Td (H) Tj ET", [0, 0, 60, 60]);
    let alpha = doc.add_object(PdfObject::Real(0.5)).number;
    let font = doc
        .add_object(object("<< /Subtype /Type1 /BaseFont /Helvetica >>"))
        .number;
    resources(
        &mut doc,
        &format!("<< /ExtGState << /G << /ca {alpha} 0 R /Font [{font} 0 R 40] >> >> >>"),
    );
    let page = render(&doc);
    assert!(page
        .pixels
        .chunks_exact(4)
        .any(|p| p[0] >= 127 && p[0] <= 130));
    assert!(page.pixels.chunks_exact(4).all(|p| p[0] >= 127));
    assert!(!page.warnings.iter().any(|w| w.contains("missing font")));
}

#[test]
fn excessive_saved_states_skip_only_the_nested_content() {
    let content = format!(
        "{}1 0 0 rg 0 0 40 40 re f {}0 0 1 rg 0 0 10 10 re f",
        "q ".repeat(140),
        "Q ".repeat(140)
    );
    let page = render(&doc_with_content(&content, [0, 0, 40, 40]));
    assert!(page
        .warnings
        .iter()
        .any(|w| w.contains("graphics-state nesting")));
    assert_eq!(pixel(&page, 20, 20), (255, 255, 255));
    assert_eq!(pixel(&page, 5, 35), (0, 0, 255));
}

#[test]
fn recursive_soft_mask_is_bounded_and_reported() {
    let mut doc = doc_with_content("/M gs 0 0 40 40 re f", [0, 0, 40, 40]);
    let id = ObjectId::new(99, 0);
    let content = b"/M gs 0 0 40 40 re f";
    doc.set_object(id,PdfObject::Stream(PdfStream::new(object("<< /Subtype /Form /BBox [0 0 40 40] /Group << /S /Transparency /I true >> /Resources << /ExtGState << /M << /SMask << /S /Alpha /G 99 0 R >> >> >> >> >>").as_dict().unwrap().clone(),content.to_vec())));
    resources(
        &mut doc,
        "<< /ExtGState << /M << /SMask << /S /Alpha /G 99 0 R >> >> >> >>",
    );
    let page = render(&doc);
    assert!(page
        .warnings
        .iter()
        .any(|w| w.contains("recursive content")));
}
