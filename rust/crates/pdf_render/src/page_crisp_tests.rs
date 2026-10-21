//! Rules, borders and clips land on whole pixels the way PDFium and Acrobat
//! draw them, so a page reads as crisply here as it does in Drive.
use super::tests::{doc_with_content, pixel};
use super::*;

fn render(content: &str) -> RenderedPage {
    render_page(
        &doc_with_content(content, [0, 0, 100, 40]),
        0,
        RenderOptions::default(),
    )
    .unwrap()
}

fn darkness(page: &RenderedPage, x: u32, y: u32) -> u8 {
    255 - pixel(page, x, y).0
}

#[test]
fn a_thin_filled_rule_is_one_solid_row() {
    // Half a pixel tall, between device rows: anti-aliased this was two
    // faint grey rows.
    let page = render("0 g 10 20.3 80 0.5 re f");
    assert_eq!(pixel(&page, 50, 19), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 18), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 20), (255, 255, 255));
}

#[test]
fn a_rule_is_as_many_pixels_as_its_width_rounds_to() {
    // 1.45 pixels tall (device rows 19.3 to 20.75): one solid row, as
    // Acrobat draws it, not two.
    let page = render("0 g 10 19.25 80 1.45 re f");
    assert_eq!(pixel(&page, 50, 20), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 19), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 21), (255, 255, 255));
}

#[test]
fn a_rectangle_with_no_height_still_shows() {
    let page = render("0 g 10 20.5 80 0 re f");
    assert_eq!(pixel(&page, 50, 19), (0, 0, 0));
}

#[test]
fn a_snapped_rectangle_keeps_the_pixels_it_covers_most() {
    // Device rows 10.6 to 12.2: 1.6 pixels tall becomes two whole rows, and
    // the barely-touched row 12 is the one left out.
    let page = render("0 g 10 27.8 80 1.6 re f");
    assert_eq!(pixel(&page, 50, 10), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 11), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 12), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 9), (255, 255, 255));
}

#[test]
fn shapes_other_than_lone_rectangles_stay_anti_aliased() {
    // Two rectangles in one fill, and a slanted quadrilateral.
    let page = render("0 g 10 20.3 30 0.5 re 50 20.3 30 0.5 re f");
    assert!(darkness(&page, 20, 19) < 200, "{:?}", pixel(&page, 20, 19));
    let page = render("0 g 10 20 m 90 21 l 90 21.5 l 10 20.5 l f");
    assert!((1..255).contains(&darkness(&page, 50, 19)));
}

#[test]
fn hairline_strokes_are_at_least_one_solid_pixel() {
    // 0.2 units wide, centred between two pixel rows.
    let page = render("0 G 0.2 w 10 20.2 m 90 20.2 l S");
    assert_eq!(pixel(&page, 50, 19), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 20), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 18), (255, 255, 255));
}

#[test]
fn rectangles_stroke_on_whole_pixels() {
    let page = render("0 G 1 w 10.3 10.3 60.4 20.4 re S");
    for (x, y) in [(40, 9), (40, 29), (10, 20), (70, 20)] {
        assert_eq!(pixel(&page, x, y), (0, 0, 0), "edge pixel ({x}, {y})");
    }
    assert_eq!(pixel(&page, 40, 20), (255, 255, 255));
}

#[test]
fn slanted_hairlines_stay_visible() {
    // A one-pixel line crossing between rows is about half dark in each;
    // at its own 0.1 width it would barely tint them.
    let page = render("0 G 0.1 w 10 10 m 90 30 l S");
    let darkest = (0..40).map(|y| darkness(&page, 50, y)).max().unwrap();
    assert!(darkest >= 100, "darkest pixel only {darkest}");
}

#[test]
fn text_outlines_are_not_thinner_than_a_pixel() {
    let mut doc = doc_with_content(
        "0 G 0.05 w BT /F 30 Tf 1 Tr 10 8 Td (I) Tj ET",
        [0, 0, 100, 40],
    );
    let font = doc.add_object(
        pdf_core::parser::Parser::new(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
            .parse_object()
            .unwrap(),
    );
    let id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
    page.insert(
        "Resources".into(),
        PdfObject::Dictionary(Dictionary::from([(
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([("F".into(), PdfObject::Reference(font))])),
        )])),
    );
    doc.set_object(id, PdfObject::Dictionary(page));
    let page = render_page(&doc, 0, RenderOptions::default()).unwrap();
    let ink: u32 = page
        .pixels
        .chunks_exact(4)
        .map(|p| u32::from(255 - p[0]))
        .sum();
    // A 30pt I's outline is about 45 pixels round: at a twentieth of a pixel
    // wide it would leave the ink of two or three.
    assert!(ink > 255 * 25, "ink {ink}");
}

#[test]
fn rectangular_clips_widen_to_whole_pixels() {
    // Clip rows 9.5 to 29.5: the half-covered rows are kept whole, so what
    // the clip frames has a solid edge instead of a grey fringe.
    let page = render("10 10.5 80 20 re W n 0 g 0 0 100 40 re f");
    assert_eq!(pixel(&page, 50, 9), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 29), (0, 0, 0));
    assert_eq!(pixel(&page, 50, 8), (255, 255, 255));
    assert_eq!(pixel(&page, 50, 30), (255, 255, 255));
}

#[test]
fn a_region_is_the_same_pixels_as_a_full_render() {
    let doc = doc_with_content(
        "0 g 10 20.3 80 0.5 re f 0 0 1 rg 30 5 m 70 35 l 50 2 l f \
         0 G 0.5 w 5 5 m 95 38 l S 1 0 0 RG 2 w 20 30 m 80 30 l S",
        [0, 0, 100, 40],
    );
    let size = RenderSize::FitBox {
        width: 300,
        height: 120,
    };
    let full = render_page(
        &doc,
        0,
        RenderOptions {
            size,
            ..Default::default()
        },
    )
    .unwrap();
    let region = PixelRegion {
        x: 37,
        y: 11,
        width: 150,
        height: 70,
    };
    let part = render_page(
        &doc,
        0,
        RenderOptions {
            size,
            region: Some(region),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((part.width, part.height), (150, 70));
    for y in 0..70 {
        for x in 0..150 {
            let (a, b) = (pixel(&part, x, y), pixel(&full, x + 37, y + 11));
            let close = |p: u8, q: u8| p.abs_diff(q) <= 1;
            assert!(
                close(a.0, b.0) && close(a.1, b.1) && close(a.2, b.2),
                "({x}, {y}): region {a:?}, page {b:?}"
            );
        }
    }
}

#[test]
fn a_region_snaps_rectangles_exactly_as_the_page_does() {
    // At 5.76 pixels a point this square's edges snap from x.5: in a region
    // lower down the page the same edge is at a negative half, which rounding
    // away from zero sent a row the other way.
    let doc = doc_with_content("0 g 10 10 80 80 re f", [0, 0, 200, 100]);
    let size = RenderSize::FitBox {
        width: 1152,
        height: 576,
    };
    let full = render_page(
        &doc,
        0,
        RenderOptions {
            size,
            ..Default::default()
        },
    )
    .unwrap();
    let region = PixelRegion {
        x: 0,
        y: 512,
        width: 1152,
        height: 64,
    };
    let part = render_page(
        &doc,
        0,
        RenderOptions {
            size,
            region: Some(region),
            ..Default::default()
        },
    )
    .unwrap();
    for y in 0..64 {
        assert_eq!(
            pixel(&part, 300, y),
            pixel(&full, 300, y + 512),
            "row {}",
            y + 512
        );
    }
}

#[test]
fn regions_are_clipped_to_the_page_and_budgeted_on_their_own() {
    let doc = doc_with_content("0 g 0 0 100 40 re f", [0, 0, 100, 40]);
    // The page at this size is four gigapixels; the region is all that is
    // allocated, so only it has to fit the budget.
    let options = |region| RenderOptions {
        size: RenderSize::FitBox {
            width: 100_000,
            height: 40_000,
        },
        max_pixels: 10_000,
        region: Some(region),
        ..Default::default()
    };
    let corner = PixelRegion {
        x: 99_950,
        y: 39_990,
        width: 100,
        height: 100,
    };
    let part = render_page(&doc, 0, options(corner)).unwrap();
    assert_eq!((part.width, part.height), (50, 10));
    assert_eq!(pixel(&part, 10, 5), (0, 0, 0));
    let outside = PixelRegion {
        x: 100_000,
        y: 0,
        width: 10,
        height: 10,
    };
    assert!(render_page(&doc, 0, options(outside)).is_err());
    let too_big = PixelRegion {
        x: 0,
        y: 0,
        width: 200,
        height: 200,
    };
    assert!(render_page(&doc, 0, options(too_big)).is_err());
}

#[test]
fn a_clip_with_no_area_still_shows_nothing() {
    let page = render("10.5 10 0 20 re W n 0 g 0 0 100 40 re f");
    assert!(page
        .pixels
        .chunks_exact(4)
        .all(|p| p[..3] == [255, 255, 255]));
}
