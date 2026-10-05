//! Raster regressions for content which previously vanished or became black.
use super::tests::{doc_with_content, pixel};
use super::*;
use pdf_core::parser::Parser;
use pdf_core::stream::PdfStream;

fn object(source: &str) -> PdfObject {
    Parser::new(source.as_bytes()).parse_object().unwrap()
}

fn set_page(doc: &mut PdfDocument, key: &str, value: PdfObject) {
    let id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
    page.insert(key.into(), value);
    doc.set_object(id, PdfObject::Dictionary(page));
}

fn resources(doc: &mut PdfDocument, source: &str) {
    set_page(doc, "Resources", object(source));
}

fn render(doc: &PdfDocument) -> RenderedPage {
    render_page(doc, 0, RenderOptions::default()).unwrap()
}

fn appearance(doc: &mut PdfDocument, dictionary: &str, content: &str) -> PdfObject {
    let dictionary = object(dictionary).as_dict().unwrap().clone();
    PdfObject::Reference(doc.add_object(PdfObject::Stream(PdfStream::new(
        dictionary,
        content.as_bytes().to_vec(),
    ))))
}

fn annotation(doc: &mut PdfDocument, dictionary: &str, ap: Option<PdfObject>) {
    let mut dict = object(dictionary).as_dict().unwrap().clone();
    if let Some(ap) = ap {
        dict.insert(
            "AP".into(),
            PdfObject::Dictionary(Dictionary::from([("N".into(), ap)])),
        );
    }
    let id = doc.add_object(PdfObject::Dictionary(dict));
    set_page(
        doc,
        "Annots",
        PdfObject::Array(vec![PdfObject::Reference(id)]),
    );
}

#[test]
fn axial_gradient_interpolates_and_respects_clip() {
    let mut doc = doc_with_content("10 10 80 40 re W n /S sh", [0, 0, 100, 60]);
    resources(&mut doc, "<< /Shading << /S << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >> >>");
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 5, 30), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 5), (255, 255, 255));
    let left = pixel(&page, 15, 30);
    let right = pixel(&page, 85, 30);
    assert!(left.0 > 200 && left.2 < 50, "{left:?}");
    assert!(right.2 > 200 && right.0 < 50, "{right:?}");
}

#[test]
fn radial_gradient_leaves_outside_circles_unpainted() {
    let mut doc = doc_with_content("/S sh", [0, 0, 100, 100]);
    resources(&mut doc, "<< /Shading << /S << /ShadingType 3 /ColorSpace /DeviceRGB /Coords [50 50 0 50 50 35] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >> >>");
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert!(pixel(&page, 50, 50).0 > 240);
    assert!(pixel(&page, 83, 50).2 > 230);
    assert_eq!(pixel(&page, 95, 50), (255, 255, 255));
}

#[test]
fn shading_pattern_matrix_uses_default_space_and_clips_to_the_shape() {
    let mut doc = doc_with_content(
        "/Pattern cs /P scn 1 0 0 1 10 0 cm 10 5 40 30 re f",
        [0, 0, 80, 40],
    );
    resources(&mut doc, "<< /Pattern << /P << /PatternType 2 /Matrix [2 0 0 1 0 0] /Shading << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 40 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >> >> >>");
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 15, 20), (255, 255, 255));
    let middle = pixel(&page, 40, 20);
    assert!(
        (120..=135).contains(&middle.0) && (120..=135).contains(&middle.2),
        "{middle:?}"
    );
    assert_eq!(pixel(&page, 65, 20), (255, 255, 255));
}

#[test]
fn shading_function_range_and_pattern_background_are_honoured() {
    let mut doc = doc_with_content("/Pattern cs /P scn 0 0 100 30 re f", [0, 0, 100, 30]);
    resources(&mut doc, "<< /Pattern << /P << /PatternType 2 /Shading << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [20 0 80 0] /Background [0 1 0] /Function << /FunctionType 2 /Domain [0 1] /Range [0 0.5 0 1 0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >> >> >>");
    let page = render(&doc);
    assert_eq!(pixel(&page, 10, 15), (0, 255, 0));
    assert_eq!(pixel(&page, 90, 15), (0, 255, 0));
    assert!((125..=129).contains(&pixel(&page, 25, 15).0));
}

#[test]
fn stitching_function_and_extended_endpoints_render() {
    let mut doc = doc_with_content("/S sh", [0, 0, 100, 20]);
    resources(&mut doc, "<< /Shading << /S << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [20 0 80 0] /Extend [true true] /Function << /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 0 1] /Functions [<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 1 0] /N 1 >> << /FunctionType 2 /Domain [0 1] /C0 [0 1 0] /C1 [0 0 1] /N 1 >>] >> >> >> >>");
    let page = render(&doc);
    assert_eq!(pixel(&page, 5, 10), (255, 0, 0));
    assert_eq!(pixel(&page, 95, 10), (0, 0, 255));
    assert!(pixel(&page, 50, 10).1 > 240);
}

fn tiled_doc(uncoloured: bool) -> PdfDocument {
    let mut doc = doc_with_content(
        if uncoloured {
            "/PCS cs 0 1 0 /P scn 0 0 40 40 re f"
        } else {
            "/Pattern cs /P scn 0 0 40 40 re f"
        },
        [0, 0, 40, 40],
    );
    let tile = appearance(&mut doc, &format!("<< /PatternType 1 /PaintType {} /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources <<>> >>", if uncoloured {2} else {1}), if uncoloured {"0 0 5 10 re f"} else {"1 0 0 rg 0 0 5 10 re f"});
    let mut res = object("<< /ColorSpace << /PCS [/Pattern /DeviceRGB] >> >>")
        .as_dict()
        .unwrap()
        .clone();
    res.insert(
        "Pattern".into(),
        PdfObject::Dictionary(Dictionary::from([("P".into(), tile)])),
    );
    set_page(&mut doc, "Resources", PdfObject::Dictionary(res));
    doc
}

#[test]
fn coloured_tiles_repeat_without_black_boxes() {
    let page = render(&tiled_doc(false));
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    for x in [2, 12, 22, 32] {
        assert_eq!(pixel(&page, x, 15), (255, 0, 0));
    }
    for x in [7, 17, 27, 37] {
        assert_eq!(pixel(&page, x, 15), (255, 255, 255));
    }
}

#[test]
fn uncoloured_tiles_use_the_supplied_base_colour() {
    let page = render(&tiled_doc(true));
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 22, 15), (0, 255, 0));
    assert_eq!(pixel(&page, 27, 15), (255, 255, 255));
}

#[test]
fn malformed_mesh_pattern_is_reported_instead_of_filling_black() {
    let mut doc = doc_with_content("/Pattern cs /P scn 0 0 40 40 re f", [0, 0, 40, 40]);
    resources(&mut doc,"<< /Pattern << /P << /PatternType 2 /Shading << /ShadingType 4 /ColorSpace /DeviceRGB >> >> >> >>");
    let page = render(&doc);
    assert_eq!(pixel(&page, 20, 20), (255, 255, 255));
    assert!(
        page.warnings
            .iter()
            .any(|w| w.contains("mesh shading skipped: missing data stream")),
        "{:?}",
        page.warnings
    );
}

#[test]
fn inline_raw_and_filtered_images_render_and_preserve_following_operators() {
    for bytes in [
        b"q 20 0 0 20 0 0 cm BI /W 1 /H 1 /CS /RGB /BPC 8 ID \xff\x00\x00 EI Q 0 0 1 rg 20 0 20 20 re f".to_vec(),
        b"q 20 0 0 20 0 0 cm BI /W 1 /H 1 /CS /RGB /BPC 8 /F /AHx ID FF0000> EI Q 0 0 1 rg 20 0 20 20 re f".to_vec(),
    ] {
        let mut doc = doc_with_content("", [0,0,40,20]);
        let id = doc.add_object(PdfObject::Stream(PdfStream::new(Dictionary::new(),bytes)));
        set_page(&mut doc,"Contents",PdfObject::Reference(id));
        let page = render(&doc);
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert_eq!(pixel(&page,10,10), (255,0,0));
        assert_eq!(pixel(&page,30,10), (0,0,255));
    }
}

#[test]
fn truncated_inline_images_are_not_silently_dropped() {
    let doc = doc_with_content(
        "1 0 0 rg 0 0 20 20 re f BI /W 10 /H 10 /CS /G /BPC 8 ID short",
        [0, 0, 20, 20],
    );
    let page = render(&doc);
    assert_eq!(pixel(&page, 10, 10), (255, 0, 0));
    assert!(
        page.warnings.iter().any(|w| w.contains("inline image")),
        "{:?}",
        page.warnings
    );
}

#[test]
fn annotation_appearance_maps_its_transformed_bbox_to_rect() {
    let mut doc = doc_with_content("", [0, 0, 100, 100]);
    let ap = appearance(
        &mut doc,
        "<< /Subtype /Form /BBox [0 0 10 20] /Matrix [0 1 -1 0 20 0] >>",
        "1 0 0 rg 0 0 10 20 re f",
    );
    annotation(
        &mut doc,
        "<< /Subtype /Stamp /Rect [20 30 60 50] >>",
        Some(ap),
    );
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 40, 60), (255, 0, 0));
    assert_eq!(pixel(&page, 10, 60), (255, 255, 255));
    assert_eq!(pixel(&page, 40, 40), (255, 255, 255));
}

#[test]
fn signature_appearance_is_drawn_as_artwork() {
    let mut doc = doc_with_content("", [0, 0, 60, 30]);
    let ap = appearance(
        &mut doc,
        "<< /Subtype /Form /BBox [0 0 20 10] >>",
        "0 0 1 rg 0 0 20 10 re f",
    );
    annotation(
        &mut doc,
        "<< /Subtype /Widget /FT /Sig /Rect [10 10 50 20] >>",
        Some(ap),
    );
    let page = render(&doc);
    assert_eq!(pixel(&page, 30, 15), (0, 0, 255));
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
}

#[test]
fn hidden_and_no_view_annotations_are_not_painted_or_warned() {
    for flag in [2, 32] {
        let mut doc = doc_with_content("", [0, 0, 40, 40]);
        let ap = appearance(
            &mut doc,
            "<< /BBox [0 0 10 10] >>",
            "1 0 0 rg 0 0 10 10 re f",
        );
        annotation(
            &mut doc,
            &format!("<< /Subtype /Stamp /F {flag} /Rect [0 0 40 40] >>"),
            Some(ap),
        );
        let page = render(&doc);
        assert_eq!(pixel(&page, 20, 20), (255, 255, 255));
        assert!(page.warnings.is_empty());
    }
}

#[test]
fn widget_appearance_uses_inherited_value_and_local_state_wins() {
    for state in ["", "/AS /Off"] {
        let mut doc = doc_with_content("", [0, 0, 30, 30]);
        let on = appearance(
            &mut doc,
            "<< /BBox [0 0 10 10] >>",
            "1 0 0 rg 0 0 10 10 re f",
        );
        let off = appearance(
            &mut doc,
            "<< /BBox [0 0 10 10] >>",
            "0 0 1 rg 0 0 10 10 re f",
        );
        let parent = doc.add_object(object("<< /FT /Btn /V /Yes >>"));
        let ap = PdfObject::Dictionary(Dictionary::from([("Yes".into(), on), ("Off".into(), off)]));
        annotation(
            &mut doc,
            &format!(
                "<< /Subtype /Widget /Parent {} 0 R {state} /Rect [0 0 30 30] >>",
                parent.number
            ),
            Some(ap),
        );
        let page = render(&doc);
        assert_eq!(
            pixel(&page, 15, 15),
            if state.is_empty() {
                (255, 0, 0)
            } else {
                (0, 0, 255)
            }
        );
    }
}

#[test]
fn highlight_without_appearance_multiplies_backdrop_and_preserves_black_text() {
    for quad in ["[10 30 30 30 10 10 30 10]", "[10 10 30 10 30 30 10 30]"] {
        let mut doc = doc_with_content("0 0 0 rg 18 10 4 20 re f", [0, 0, 40, 40]);
        annotation(
            &mut doc,
            &format!("<< /Subtype /Highlight /Rect [10 10 30 30] /QuadPoints {quad} /C [1 1 0] >>"),
            None,
        );
        let page = render(&doc);
        assert_eq!(pixel(&page, 15, 20), (255, 255, 0));
        assert_eq!(pixel(&page, 20, 20), (0, 0, 0));
        assert_eq!(pixel(&page, 5, 20), (255, 255, 255));
    }
}

#[test]
fn missing_text_field_appearance_draws_inherited_value_and_warns() {
    let mut doc = doc_with_content("", [0, 0, 100, 40]);
    let parent = doc.add_object(object("<< /FT /Tx /V (Hello) /DA (/F1 18 Tf 0 0 1 rg) >>"));
    annotation(
        &mut doc,
        &format!(
            "<< /Subtype /Widget /Parent {} 0 R /Rect [10 5 90 35] >>",
            parent.number
        ),
        None,
    );
    let page = render(&doc);
    assert!(
        page.pixels
            .chunks_exact(4)
            .any(|p| p[0] < 128 && p[1] < 128 && p[2] > 240),
        "field value should be visible"
    );
    assert!(
        page.warnings
            .iter()
            .any(|w| w.contains("approximate layout")),
        "{:?}",
        page.warnings
    );
    assert_eq!(pixel(&page, 5, 20), (255, 255, 255));
}

#[test]
fn unsupported_annotation_and_blend_mode_warn() {
    let mut doc = doc_with_content("/G gs 1 0 0 rg 0 0 20 20 re f", [0, 0, 20, 20]);
    resources(
        &mut doc,
        "<< /ExtGState << /G << /BM /UnknownBlend >> >> >>",
    );
    annotation(&mut doc, "<< /Subtype /3D /Rect [0 0 20 20] >>", None);
    let page = render(&doc);
    assert_eq!(pixel(&page, 10, 10), (255, 0, 0));
    assert!(page.warnings.iter().any(|w| w.contains("blend mode")));
    assert!(page.warnings.iter().any(|w| w.contains("3D annotation")));
}

#[test]
fn recovery_warnings_reach_rendered_page_callers() {
    let mut doc = doc_with_content("1 0 0 rg 0 0 20 20 re f", [0, 0, 20, 20]);
    doc.recovery_warnings
        .push("Damaged cross-reference table was recovered".into());
    let page = render(&doc);
    assert_eq!(pixel(&page, 10, 10), (255, 0, 0));
    assert!(page.warnings.iter().any(|w| w.contains("cross-reference")));
}

#[test]
fn named_pattern_colour_space_alias_does_not_fall_back_to_black() {
    let mut doc = tiled_doc(false);
    let content = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        b"/Alias cs /P scn 0 0 40 40 re f".to_vec(),
    )));
    set_page(&mut doc, "Contents", PdfObject::Reference(content));
    let page_id = doc.collect_page_ids().unwrap()[0];
    let mut res = doc
        .resolve(page_id)
        .unwrap()
        .as_dict()
        .unwrap()
        .get("Resources")
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    res.insert("ColorSpace".into(), object("<< /Alias /Pattern >>"));
    set_page(&mut doc, "Resources", PdfObject::Dictionary(res));
    let page = render(&doc);
    assert_eq!(pixel(&page, 22, 15), (255, 0, 0));
    assert_eq!(pixel(&page, 27, 15), (255, 255, 255));
    assert!(page.warnings.is_empty());
}

#[test]
fn widget_appearance_uses_acroform_default_font_resources() {
    let mut doc = doc_with_content("", [0, 0, 100, 40]);
    let catalog_id = doc.root_ref().unwrap();
    let mut catalog = doc.catalog().unwrap().clone();
    catalog.insert("AcroForm".into(), object("<< /DR << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>"));
    doc.set_object(catalog_id, PdfObject::Dictionary(catalog));
    let ap = appearance(
        &mut doc,
        "<< /Subtype /Form /BBox [0 0 80 30] >>",
        "0 0 1 rg BT /F1 18 Tf 2 5 Td (Hello) Tj ET",
    );
    annotation(
        &mut doc,
        "<< /Subtype /Widget /FT /Tx /Rect [10 5 90 35] >>",
        Some(ap),
    );
    let page = render(&doc);
    assert!(page
        .pixels
        .chunks_exact(4)
        .any(|p| p[0] < 128 && p[1] < 128 && p[2] > 240));
    assert!(
        !page.warnings.iter().any(|w| w.contains("missing font")),
        "{:?}",
        page.warnings
    );
}

#[test]
fn invisible_flag_suppresses_only_unknown_annotation_types() {
    for subtype in ["Stamp", "CustomUnknown"] {
        let mut doc = doc_with_content("", [0, 0, 40, 40]);
        let ap = appearance(
            &mut doc,
            "<< /BBox [0 0 10 10] >>",
            "1 0 0 rg 0 0 10 10 re f",
        );
        annotation(
            &mut doc,
            &format!("<< /Subtype /{subtype} /F 1 /Rect [0 0 40 40] >>"),
            Some(ap),
        );
        let page = render(&doc);
        assert_eq!(
            pixel(&page, 20, 20),
            if subtype == "Stamp" {
                (255, 0, 0)
            } else {
                (255, 255, 255)
            }
        );
        assert!(page.warnings.is_empty());
    }
}

#[test]
fn dashed_strokes_honour_phase_scale_and_subpath_restart() {
    let doc = doc_with_content("2 w [8 8] 4 d 4 10 m 60 10 l S q 2 0 0 1 0 0 cm 2 20 m 30 20 l S Q 4 30 m 60 30 l 4 40 m 60 40 l S", [0, 0, 80, 50]);
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    for y in [10, 30, 40] {
        assert_eq!(pixel(&page, 5, 50 - y), (0, 0, 0));
        assert_eq!(pixel(&page, 10, 50 - y), (255, 255, 255));
        assert_eq!(pixel(&page, 20, 50 - y), (0, 0, 0));
    }
    assert_eq!(pixel(&page, 10, 30), (0, 0, 0));
    assert_eq!(pixel(&page, 20, 30), (255, 255, 255));
    assert_eq!(pixel(&page, 30, 30), (0, 0, 0));
}

#[test]
fn odd_dash_patterns_extgstate_and_zero_length_round_dots_render() {
    let mut doc = doc_with_content(
        "/G gs 5 10 m 75 10 l S 1 J [0 12] 0 d 5 25 m 75 25 l S",
        [0, 0, 80, 40],
    );
    resources(
        &mut doc,
        "<< /ExtGState << /G << /D [[8] 0] /LW 4 /LC 0 >> >> >>",
    );
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    for (x, y, black) in [
        (8, 30, true),
        (16, 30, false),
        (24, 30, true),
        (5, 15, true),
        (17, 15, true),
        (11, 15, false),
    ] {
        assert_eq!(
            pixel(&page, x, y),
            if black { (0, 0, 0) } else { (255, 255, 255) }
        );
    }
}

#[test]
fn excessive_dash_detail_is_bounded_and_warned() {
    let doc = doc_with_content("2 w [0.00001 0.00001] 0 d 0 10 m 20 10 l S", [0, 0, 20, 20]);
    assert!(render(&doc)
        .warnings
        .iter()
        .any(|w| w.contains("dash detail")));
}

fn layered(content: &str) -> (PdfDocument, PdfObject, PdfObject) {
    let mut doc = doc_with_content(content, [0, 0, 80, 40]);
    let hidden = PdfObject::Reference(doc.add_object(object("<< /Type /OCG /Name (hidden) >>")));
    let shown = PdfObject::Reference(doc.add_object(object("<< /Type /OCG /Name (shown) >>")));
    resources(
        &mut doc,
        &format!(
            "<< /Properties << /Hidden {} 0 R /Shown {} 0 R >> >>",
            hidden.as_ref().unwrap().number,
            shown.as_ref().unwrap().number
        ),
    );
    let catalog_id = doc.root_ref().unwrap();
    let mut catalog = doc.catalog().unwrap().clone();
    catalog.insert(
        "OCProperties".into(),
        object(&format!(
            "<< /OCGs [{} 0 R {} 0 R] /D << /OFF [{} 0 R] >> >>",
            hidden.as_ref().unwrap().number,
            shown.as_ref().unwrap().number,
            hidden.as_ref().unwrap().number
        )),
    );
    doc.set_object(catalog_id, PdfObject::Dictionary(catalog));
    (doc, hidden, shown)
}

fn replace_layer_configuration(doc: &mut PdfDocument, key: &str, value: PdfObject) {
    let root = doc.root_ref().unwrap();
    let mut catalog = doc.catalog().unwrap().clone();
    let mut properties = doc.resolve_dict(&catalog["OCProperties"]).unwrap().clone();
    properties.insert(key.into(), value);
    catalog.insert("OCProperties".into(), PdfObject::Dictionary(properties));
    doc.set_object(root, PdfObject::Dictionary(catalog));
}

#[test]
fn hidden_content_keeps_state_and_nested_content_stays_hidden() {
    let (doc, _, _) = layered("/OC /Hidden BDC 1 0 0 rg 0 0 80 40 re f /OC /Shown BDC 0 0 80 40 re f EMC EMC 50 0 30 40 re f");
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 10, 20), (255, 255, 255));
    assert_eq!(pixel(&page, 60, 20), (255, 0, 0));
}

#[test]
fn layer_membership_policies_and_expressions_honour_hidden_groups() {
    for (policy, visible) in [
        ("AnyOn", true),
        ("AllOn", false),
        ("AnyOff", true),
        ("AllOff", false),
    ] {
        let (mut doc, hidden, shown) = layered("/OC /Member BDC 1 0 0 rg 0 0 80 40 re f EMC");
        let h = hidden.as_ref().unwrap().number;
        let s = shown.as_ref().unwrap().number;
        resources(&mut doc, &format!("<< /Properties << /Member << /Type /OCMD /P /{policy} /OCGs [{h} 0 R {s} 0 R] >> >> >>"));
        let page = render(&doc);
        assert_eq!(
            pixel(&page, 20, 20),
            if visible {
                (255, 0, 0)
            } else {
                (255, 255, 255)
            },
            "{policy}"
        );
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        resources(&mut doc, &format!("<< /Properties << /Member << /Type /OCMD /P /{policy} /VE [/And [/Not {h} 0 R] {s} 0 R] >> >> >>"));
        assert_eq!(pixel(&render(&doc), 20, 20), (255, 0, 0), "VE overrides P");
    }
}

#[test]
fn unrelated_layer_intents_do_not_hide_membership_content() {
    for policy in ["AnyOn", "AllOn", "AnyOff", "AllOff"] {
        let (mut doc, hidden, _) = layered("/OC /Member BDC 1 0 0 rg 0 0 80 40 re f EMC");
        let id = hidden.as_ref().unwrap();
        doc.set_object(
            id,
            object("<< /Type /OCG /Name (design) /Intent /Design >>"),
        );
        resources(
            &mut doc,
            &format!(
                "<< /Properties << /Member << /Type /OCMD /P /{policy} /OCGs [{} 0 R] >> >> >>",
                id.number
            ),
        );
        assert_eq!(pixel(&render(&doc), 20, 20), (255, 0, 0), "{policy}");
    }
}

#[test]
fn view_usage_applies_only_to_named_groups_and_unsupported_usage_warns() {
    let (mut doc, hidden, _) = layered("/OC /Hidden BDC 1 0 0 rg 0 0 80 40 re f EMC");
    let id = hidden.as_ref().unwrap();
    doc.set_object(
        id,
        object("<< /Type /OCG /Name (hidden) /Usage << /View << /ViewState /ON >> >> >>"),
    );
    for groups in ["".to_owned(), format!("/OCGs [{} 0 R]", id.number)] {
        replace_layer_configuration(
            &mut doc,
            "D",
            object(&format!(
                "<< /OFF [{} 0 R] /AS [<< /Event /View /Category [/View] {groups} >>] >>",
                id.number
            )),
        );
        let page = render(&doc);
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert_eq!(
            pixel(&page, 20, 20),
            if groups.is_empty() {
                (255, 255, 255)
            } else {
                (255, 0, 0)
            }
        );
    }
    replace_layer_configuration(
        &mut doc,
        "D",
        object(&format!(
            "<< /OFF [{} 0 R] /AS [<< /Event /View /Category [/View /Zoom] /OCGs [{} 0 R] >>] >>",
            id.number, id.number
        )),
    );
    let page = render(&doc);
    assert_eq!(pixel(&page, 20, 20), (255, 255, 255));
    assert!(page
        .warnings
        .iter()
        .any(|w| w.contains("environment-dependent")));
}

#[test]
fn hidden_xobjects_and_annotations_are_not_painted() {
    let (mut doc, hidden, _) = layered("/Form Do q 80 0 0 40 0 0 cm /Image Do Q");
    let id = hidden.as_ref().unwrap().number;
    let form = appearance(
        &mut doc,
        &format!("<< /Subtype /Form /BBox [0 0 80 40] /OC {id} 0 R >>"),
        "1 0 0 rg 0 0 80 40 re f",
    );
    let image = PdfObject::Reference(doc.add_object(PdfObject::Stream(PdfStream::new(object(&format!("<< /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /OC {id} 0 R >>")).as_dict().unwrap().clone(), vec![255, 0, 0]))));
    set_page(
        &mut doc,
        "Resources",
        PdfObject::Dictionary(Dictionary::from([(
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([
                ("Form".into(), form.clone()),
                ("Image".into(), image),
            ])),
        )])),
    );
    annotation(
        &mut doc,
        &format!("<< /Subtype /Stamp /Rect [0 0 80 40] /OC {id} 0 R >>"),
        Some(form),
    );
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert!(page
        .pixels
        .chunks_exact(4)
        .all(|p| p[..3] == [255, 255, 255]));
}

fn saved(doc: PdfDocument) -> PdfDocument {
    let doc = PdfDocument::from_bytes(&doc.to_bytes().unwrap()).unwrap();
    assert!(doc.recovery_warnings.is_empty());
    doc
}

#[test]
fn extraction_merge_and_duplication_preserve_layer_references_and_appearance() {
    let (source, _, _) = layered("/OC /Hidden BDC 1 0 0 rg 0 0 80 40 re f EMC");
    let (shown, _, _) = layered("/OC /Shown BDC 0 1 0 rg 0 0 80 40 re f EMC");
    let extracted = saved(pdf_ops::split::extract_pages(&source, &[0]).unwrap());
    let duplicated = saved(pdf_ops::split::extract_pages(&source, &[0, 0]).unwrap());
    let merged = saved(pdf_ops::merge::merge_documents(&[&source, &shown]).unwrap());
    for doc in [&extracted, &duplicated, &merged] {
        let props = doc
            .resolve_dict(&doc.catalog().unwrap()["OCProperties"])
            .unwrap();
        let groups = match doc.resolve_value(&props["OCGs"]) {
            PdfObject::Array(groups) => groups,
            _ => panic!("missing groups"),
        };
        let config = doc.resolve_dict(&props["D"]).unwrap();
        let off = match doc.resolve_value(&config["OFF"]) {
            PdfObject::Array(groups) => groups,
            _ => panic!("missing off list"),
        };
        for page in doc.collect_page_ids().unwrap() {
            let page = doc.resolve(page).unwrap().as_dict().unwrap();
            let res = doc.resolve_dict(&page["Resources"]).unwrap();
            let properties = doc.resolve_dict(&res["Properties"]).unwrap();
            assert!(groups.contains(&properties["Hidden"]));
            assert!(off.contains(&properties["Hidden"]));
            assert!(groups.contains(&properties["Shown"]));
            assert!(!off.contains(&properties["Shown"]));
        }
        assert_eq!(pixel(&render(doc), 20, 20), (255, 255, 255));
    }
    assert_eq!(
        pixel(
            &render_page(&merged, 1, RenderOptions::default()).unwrap(),
            20,
            20
        ),
        (0, 255, 0)
    );
    assert_eq!(
        pixel(
            &render_page(&duplicated, 1, RenderOptions::default()).unwrap(),
            20,
            20
        ),
        (255, 255, 255)
    );
}

#[test]
fn alternate_configs_survive_extraction_and_are_rejected_when_combining() {
    let (mut source, hidden, _) = layered("/OC /Hidden BDC 1 0 0 rg 0 0 80 40 re f EMC");
    replace_layer_configuration(
        &mut source,
        "Configs",
        PdfObject::Array(vec![object(&format!(
            "<< /Name (Show all) /ON [{} 0 R] >>",
            hidden.as_ref().unwrap().number
        ))]),
    );
    let extracted = saved(pdf_ops::split::extract_pages(&source, &[0]).unwrap());
    let props = extracted
        .resolve_dict(&extracted.catalog().unwrap()["OCProperties"])
        .unwrap();
    assert!(props.contains_key("Configs"));
    assert_eq!(pixel(&render(&extracted), 20, 20), (255, 255, 255));
    assert!(pdf_ops::split::extract_pages(&source, &[0, 0])
        .unwrap_err()
        .to_string()
        .contains("alternate layer"));
    assert!(pdf_ops::merge::merge_documents(&[&source, &source])
        .unwrap_err()
        .to_string()
        .contains("alternate layer"));
}

#[test]
fn branching_optional_content_cycles_are_bounded_and_reported() {
    let (mut doc, _, _) = layered("/OC /Cycle BDC 1 0 0 rg 0 0 80 40 re f EMC");
    let id = doc.add_object(PdfObject::Null);
    doc.set_object(
        id,
        object(&format!(
            "<< /Type /OCMD /VE [/And {} 0 R {} 0 R] >>",
            id.number, id.number
        )),
    );
    resources(
        &mut doc,
        &format!("<< /Properties << /Cycle {} 0 R >> >>", id.number),
    );
    assert!(render(&doc)
        .warnings
        .iter()
        .any(|w| w.contains("expression exceeds")));
}
