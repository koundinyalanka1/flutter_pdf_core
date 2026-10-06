use super::super::tests::doc_with_content;
use super::*;
use pdf_core::parser::Parser;

fn document(content: &str, graphics_state: &str) -> PdfDocument {
    let mut doc = doc_with_content(content, [0, 0, 100, 70]);
    let id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
    // Zero advance deliberately makes consecutive H glyphs coincide. The
    // glyph outline still comes from the ordinary substitute face.
    let source = format!(
        "<< /Font << /F << /Subtype /Type1 /BaseFont /Helvetica /FirstChar 72 /Widths [0] >> >> /ExtGState << /G << {graphics_state} >> /On << /TK true >> /Off << /TK false >> /Quarter << /ca 0.25 >> >> >>"
    );
    page.insert(
        "Resources".into(),
        Parser::new(source.as_bytes()).parse_object().unwrap(),
    );
    doc.set_object(id, PdfObject::Dictionary(page));
    doc
}

fn render(content: &str, graphics_state: &str) -> RenderedPage {
    render_page(
        &document(content, graphics_state),
        0,
        RenderOptions::default(),
    )
    .unwrap()
}

fn darkest(page: &RenderedPage) -> u8 {
    page.pixels.chunks_exact(4).map(|p| p[0]).min().unwrap()
}

fn assert_glyph_interior(page: &RenderedPage, expected: u8) {
    let reference = render("BT /F 40 Tf 10 10 Td (H) Tj ET", "");
    let mut count = 0;
    for (opaque, actual) in reference
        .pixels
        .chunks_exact(4)
        .zip(page.pixels.chunks_exact(4))
    {
        if opaque[..3] == [0, 0, 0] {
            assert_eq!(actual[..3], [expected; 3]);
            count += 1;
        }
    }
    assert!(count > 100);
}

#[test]
fn default_and_explicit_text_knockout_do_not_double_translucent_glyphs() {
    for setting in ["", "/TK true", "/TK false"] {
        for text in ["(HH) Tj", "(H) Tj (H) Tj", "[(H) 0 (H)] TJ"] {
            let page = render(
                &format!("/G gs BT /F 40 Tf 10 10 Td {text} ET"),
                &format!("/ca 0.5 {setting}"),
            );
            let expected = if setting == "/TK false" { 64 } else { 128 };
            assert_eq!(darkest(&page), expected, "{setting}: {text}");
            assert!(!page.warnings.iter().any(|w| w.contains("memory limit")));
        }
    }
}

#[test]
fn knockout_groups_end_at_et_and_q_restores_tk() {
    let separate = render(
        "/G gs BT /F 40 Tf 10 10 Td (H) Tj ET BT 10 10 Td (H) Tj ET",
        "/ca 0.5",
    );
    assert_eq!(darkest(&separate), 64);
    let restored = render(
        "/G gs q /Off gs Q BT /F 40 Tf 10 10 Td (HH) Tj ET",
        "/ca 0.5",
    );
    assert_eq!(darkest(&restored), 128);
}

#[test]
fn changes_inside_text_apply_per_glyph_and_persist_after_et() {
    let page = render(
        "/G gs BT /F 40 Tf 10 10 Td (H) Tj /Quarter gs (H) Tj ET 70 10 10 10 re f",
        "/ca 0.5",
    );
    // The second glyph knocks out the first at 25% opacity. ET must neither
    // reapply that opacity to the group nor restore its pre-BT value.
    assert_glyph_interior(&page, 191);
    assert_eq!(&page.pixels[(55 * 100 + 75) * 4..][..3], &[191, 191, 191]);
}

#[test]
fn opaque_first_glyph_does_not_become_backdrop_for_later_translucent_one() {
    let page = render("BT /F 40 Tf 10 10 Td (H) Tj /Quarter gs (H) Tj ET", "");
    // A gs later in BT/ET forces the initial backdrop to be retained before
    // the opaque first glyph. Otherwise that glyph would leave black ink.
    assert_glyph_interior(&page, 191);
}

#[test]
fn fill_stroke_glyphs_knock_out_the_fill_even_when_tk_is_false() {
    for tk in [true, false] {
        let settings = format!("/ca 0.25 /CA 0.5 /TK {tk}");
        let make = |mode| {
            render(
                &format!("/G gs 0 g 0 0 1 RG 4 w BT /F 40 Tf {mode} Tr 10 10 Td (H) Tj ET"),
                &settings,
            )
        };
        let fill = make(0);
        let stroke = make(1);
        let combined = make(2);
        let mut compared = 0;
        for ((f, s), actual) in fill
            .pixels
            .chunks_exact(4)
            .zip(stroke.pixels.chunks_exact(4))
            .zip(combined.pixels.chunks_exact(4))
        {
            if f[..3] == [191, 191, 191] && s[..3] == [128, 128, 255] {
                assert_eq!(
                    actual[..3],
                    s[..3],
                    "TK={tk}: stroke must cover, not darken, fill"
                );
                compared += 1;
            }
        }
        assert!(compared > 20, "must inspect actual fill/stroke overlap");
    }
}

#[test]
fn blend_mode_is_applied_to_glyphs_only_once() {
    let page = render(
        "0.5 g 0 0 100 70 re f /G gs 0.5 g BT /F 40 Tf 10 10 Td (HH) Tj ET",
        "/BM /Multiply",
    );
    assert_eq!(darkest(&page), 64);
}

#[test]
fn soft_mask_is_applied_to_glyphs_without_being_applied_again_at_et() {
    let mut doc = document(
        "/G gs BT /F 40 Tf 10 10 Td (HH) Tj ET",
        "/ca 0.5 /SMask << /S /Luminosity /G 40 0 R >>",
    );
    let dictionary = Parser::new(
        b"<< /Subtype /Form /BBox [0 0 100 70] /Group << /S /Transparency /I true /CS /DeviceGray >> >>"
    ).parse_object().unwrap().as_dict().unwrap().clone();
    doc.set_object(
        ObjectId::new(40, 0),
        PdfObject::Stream(pdf_core::stream::PdfStream::new(
            dictionary,
            b"0.5 g 0 0 100 70 re f".to_vec(),
        )),
    );
    let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
    assert_glyph_interior(&page, 191);
}

#[test]
fn unterminated_text_object_restores_the_parent_canvas() {
    let content = "/G gs BT /F 40 Tf 10 10 Td (HH) Tj";
    let closed = render(&format!("{content} ET"), "/ca 0.5");
    let recovered = render(content, "/ca 0.5");
    assert_eq!(recovered.pixels, closed.pixels);
}
