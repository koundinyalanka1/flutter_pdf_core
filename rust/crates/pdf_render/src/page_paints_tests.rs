use super::super::tests::{doc_with_content, pixel};
use super::*;
use pdf_core::parser::Parser;

fn object(source: &str) -> PdfObject {
    Parser::new(source.as_bytes()).parse_object().unwrap()
}
fn stream(doc: &mut PdfDocument, dict: &str, data: &[u8]) -> PdfObject {
    let dict = object(dict).as_dict().unwrap().clone();
    PdfObject::Reference(doc.add_object(PdfObject::Stream(PdfStream::new(dict, data.to_vec()))))
}
fn set_resources(doc: &mut PdfDocument, resources: Dictionary) {
    let id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
    page.insert("Resources".into(), PdfObject::Dictionary(resources));
    doc.set_object(id, PdfObject::Dictionary(page));
}
fn shade_resources(doc: &mut PdfDocument, shading: PdfObject, alpha: f64) {
    set_resources(
        doc,
        Dictionary::from([
            (
                "Shading".into(),
                PdfObject::Dictionary(Dictionary::from([("S".into(), shading)])),
            ),
            (
                "ExtGState".into(),
                object(&format!("<< /G << /ca {alpha} >> >>")),
            ),
        ]),
    );
}
fn render(doc: &PdfDocument) -> RenderedPage {
    render_page(doc, 0, RenderOptions::default()).unwrap()
}
fn mesh(kind: i64, extra: &str, data: &[u8], alpha: f64) -> RenderedPage {
    let mut doc = doc_with_content("/G gs /S sh", [0, 0, 100, 100]);
    let shading=stream(&mut doc,&format!("<< /ShadingType {kind} /ColorSpace /DeviceRGB /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 100 0 100 0 1 0 1 0 1] {extra} >>"),data);
    shade_resources(&mut doc, shading, alpha);
    render(&doc)
}

#[test]
fn function_shading_obeys_its_domain_matrix_and_calculator() {
    let mut doc = doc_with_content("/S sh", [0, 0, 100, 100]);
    let f = stream(
        &mut doc,
        "<< /FunctionType 4 /Domain [0 1 0 1] /Range [0 1] >>",
        b"{ pop }",
    );
    let mut shading =
        object("<< /ShadingType 1 /ColorSpace /DeviceGray /Matrix [80 0 0 60 10 20] >>")
            .as_dict()
            .unwrap()
            .clone();
    shading.insert("Function".into(), f);
    shade_resources(&mut doc, PdfObject::Dictionary(shading), 1.0);
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 5, 50), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 10), (255, 255, 255));
    assert!((62..=67).contains(&pixel(&page, 30, 50).0));
    assert!((190..=196).contains(&pixel(&page, 70, 50).0));
}
#[test]
fn axial_sampled_colour_function_renders_instead_of_disappearing() {
    let mut doc = doc_with_content("/S sh", [0, 0, 100, 100]);
    let f = stream(
        &mut doc,
        "<< /FunctionType 0 /Domain [0 1] /Range [0 1 0 1 0 1] /Size [2] /BitsPerSample 8 >>",
        &[255, 0, 0, 0, 0, 255],
    );
    let mut shading = object("<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] >>")
        .as_dict()
        .unwrap()
        .clone();
    shading.insert("Function".into(), f);
    shade_resources(&mut doc, PdfObject::Dictionary(shading), 1.0);
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    let p = pixel(&page, 50, 50);
    assert!(
        (124..=130).contains(&p.0) && (124..=130).contains(&p.2),
        "{p:?}"
    );
}
#[test]
fn free_form_triangle_interpolates_vertex_components() {
    let page = mesh(
        4,
        "",
        &[
            0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 255,
        ],
        1.0,
    );
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    let p = pixel(&page, 25, 75);
    assert!(
        (123..=131).contains(&p.0) && (60..=68).contains(&p.1) && (59..=67).contains(&p.2),
        "{p:?}"
    );
    assert_eq!(pixel(&page, 80, 20), (255, 255, 255));
}
#[test]
fn free_form_continuation_and_lattice_apply_alpha_once_at_shared_edges() {
    let data = [
        0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 255, 0, 0, 1, 255, 255, 255, 0, 0,
    ];
    let page = mesh(4, "", &data, 0.5);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    for p in [(25, 25), (50, 49), (75, 75)] {
        assert_eq!(pixel(&page, p.0, p.1), (255, 128, 128));
    }
    let data = [
        0, 0, 255, 0, 0, 255, 0, 255, 0, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0,
    ];
    let page = mesh(5, "/VerticesPerRow 2", &data, 0.5);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    for p in [(25, 25), (50, 49), (75, 75)] {
        assert_eq!(pixel(&page, p.0, p.1), (255, 128, 128));
    }
}
fn patch_data(kind: i64) -> Vec<u8> {
    let mut data = vec![0];
    for (x, y) in [
        (0, 0),
        (85, 0),
        (170, 0),
        (255, 0),
        (255, 85),
        (255, 170),
        (255, 255),
        (170, 255),
        (85, 255),
        (0, 255),
        (0, 170),
        (0, 85),
    ] {
        data.extend([x, y]);
    }
    if kind == 7 {
        for (x, y) in [(85, 85), (170, 85), (170, 170), (85, 170)] {
            data.extend([x, y]);
        }
    }
    data.extend([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
    data
}
#[test]
fn coons_and_tensor_patch_boundary_and_corner_colours_agree() {
    let mut pages = Vec::new();
    for kind in [6, 7] {
        let page = mesh(kind, "", &patch_data(kind), 1.0);
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        let center = pixel(&page, 50, 50);
        assert!(
            (124..=131).contains(&center.0)
                && (124..=131).contains(&center.1)
                && (124..=131).contains(&center.2),
            "{center:?}"
        );
        let corner = pixel(&page, 3, 96);
        assert!(
            corner.0 > 235 && corner.1 < 20 && corner.2 < 20,
            "{corner:?}"
        );
        pages.push(page);
    }
    for (a, b) in pages[0].pixels.iter().zip(&pages[1].pixels) {
        assert!((*a as i16 - *b as i16).abs() <= 1);
    }
}
#[test]
fn mesh_function_maps_scalar_values_after_interpolation() {
    let mut doc = doc_with_content("/S sh", [0, 0, 100, 100]);
    let shading=stream(&mut doc,"<< /ShadingType 5 /ColorSpace /DeviceRGB /BitsPerCoordinate 8 /BitsPerComponent 8 /VerticesPerRow 2 /Decode [0 100 0 100 0 1] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 2 >> >>",&[0,0,0,255,0,255,0,255,0,255,255,255]);
    shade_resources(&mut doc, shading, 1.0);
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    let p = pixel(&page, 50, 50);
    assert!(
        (186..=195).contains(&p.0) && (60..=69).contains(&p.2),
        "{p:?}"
    );
}
#[test]
fn malformed_meshes_warn_without_partial_paint_or_panics() {
    for (kind, extra, data) in [
        (4, "", vec![1, 0, 0, 0, 0, 0]),
        (5, "/VerticesPerRow 2", vec![0, 0, 0, 0, 0]),
        (6, "", vec![0, 1, 2, 3]),
        (7, "", vec![2, 1, 2, 3]),
    ] {
        let page = mesh(kind, extra, &data, 1.0);
        assert!(
            page.warnings
                .iter()
                .any(|w| w.contains("mesh shading skipped")),
            "{:?}",
            page.warnings
        );
        assert_eq!(pixel(&page, 50, 50), (255, 255, 255));
    }
}
#[test]
fn pattern_extgstate_applies_blend_and_alpha_without_leaking() {
    let mut doc = doc_with_content(
        "/Pattern cs /P scn 0 0 50 100 re f 0 0 1 rg 50 0 50 100 re f",
        [0, 0, 100, 100],
    );
    let resources=object("<< /Pattern << /P << /PatternType 2 /ExtGState << /ca 0.5 /BM /Multiply >> /Shading << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [1 0 0] /N 1 >> >> >> >> >>").as_dict().unwrap().clone();
    set_resources(&mut doc, resources);
    let page = render(&doc);
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    assert_eq!(pixel(&page, 25, 50), (255, 128, 128));
    assert_eq!(pixel(&page, 75, 50), (0, 0, 255));
}
#[test]
fn scalar_transfer_table_supports_calculator_and_rejects_multi_output() {
    let mut doc = doc_with_content("", [0, 0, 1, 1]);
    let f = stream(
        &mut doc,
        "<< /FunctionType 4 /Domain [0 1] /Range [0 1] >>",
        b"{ 1 exch sub }",
    );
    let values = scalar_function_table(&doc, &f).unwrap();
    assert_eq!(values.len(), 256);
    assert_eq!(values[0], 1.0);
    assert_eq!(values[255], 0.0);
    assert!(scalar_function_table(
        &doc,
        &object("<< /FunctionType 2 /Domain [0 1] /C0 [0 0] /C1 [1 1] /N 1 >>")
    )
    .is_none());
}

#[test]
fn packed_patch_continuation_reuses_edge_without_byte_padding() {
    fn push(bits: &mut Vec<bool>, value: u32, width: usize) {
        for shift in (0..width).rev() {
            bits.push(value & (1 << shift) != 0);
        }
    }
    for kind in [6, 7] {
        let mut bits = Vec::new();
        push(&mut bits, 0, 2);
        for (x, y) in [
            (0, 0),
            (40, 0),
            (80, 0),
            (120, 0),
            (120, 40),
            (120, 80),
            (120, 120),
            (80, 120),
            (40, 120),
            (0, 120),
            (0, 80),
            (0, 40),
        ] {
            push(&mut bits, x, 8);
            push(&mut bits, y, 8);
        }
        if kind == 7 {
            for (x, y) in [(40, 40), (80, 40), (80, 80), (40, 80)] {
                push(&mut bits, x, 8);
                push(&mut bits, y, 8);
            }
        }
        for _ in 0..4 {
            for c in [255, 0, 0] {
                push(&mut bits, c, 8);
            }
        }
        assert_eq!(bits.len() % 8, 2);
        push(&mut bits, 1, 2);
        for (x, y) in [
            (160, 120),
            (200, 120),
            (240, 120),
            (240, 80),
            (240, 40),
            (240, 0),
            (200, 0),
            (160, 0),
        ] {
            push(&mut bits, x, 8);
            push(&mut bits, y, 8);
        }
        if kind == 7 {
            for (x, y) in [(160, 40), (160, 80), (200, 80), (200, 40)] {
                push(&mut bits, x, 8);
                push(&mut bits, y, 8);
            }
        }
        for _ in 0..2 {
            for c in [0, 0, 255] {
                push(&mut bits, c, 8);
            }
        }
        let mut data = vec![0u8; bits.len().div_ceil(8)];
        for (i, b) in bits.iter().enumerate() {
            if *b {
                data[i / 8] |= 1 << (7 - i % 8);
            }
        }
        let mut doc = doc_with_content("/S sh", [0, 0, 240, 120]);
        let shading=stream(&mut doc,&format!("<< /ShadingType {kind} /ColorSpace /DeviceRGB /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 2 /Decode [0 255 0 255 0 1 0 1 0 1] >>"),&data);
        shade_resources(&mut doc, shading, 1.0);
        let page = render(&doc);
        assert!(page.warnings.is_empty(), "kind {kind}: {:?}", page.warnings);
        assert_eq!(pixel(&page, 50, 60), (255, 0, 0));
        let p = pixel(&page, 200, 60);
        assert!(
            (78..=90).contains(&p.0) && (164..=177).contains(&p.2),
            "kind {kind}: {p:?}"
        );
    }
}
