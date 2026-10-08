//! Pages scanned sideways or upside down are turned upright before reading,
//! and their word boxes still land where the words are on the page as
//! scanned.

mod common;

use common::{character_error_rate, page_image, single_spaced, LINES};
use pdf_ocr::{OcrEngine, OcrOptions};

/// Where a point of the upright page lands after `turns` clockwise quarter
/// turns of a `width` x `height` image.
fn forward(turns: u8, (width, height): (f64, f64), (x, y): (f64, f64)) -> (f64, f64) {
    match turns % 4 {
        0 => (x, y),
        1 => (height - y, x),
        2 => (width - x, height - y),
        _ => (y, width - x),
    }
}

#[test]
fn sideways_and_upside_down_pages_read_like_upright_ones() {
    let engine = OcrEngine::embedded();
    let upright = page_image(150.0);
    let reference = engine.recognize(&upright, &OcrOptions::default());
    assert_eq!(reference.orientation_degrees, 0);
    let truth = single_spaced(&LINES.join(" "));
    let first = &reference.lines[0].words[0];
    let size = (upright.width as f64, upright.height as f64);

    for turns in 1..4u8 {
        let page = engine.recognize(&upright.turned(turns), &OcrOptions::default());
        assert_eq!(
            page.orientation_degrees,
            u16::from((4 - turns) % 4) * 90,
            "turned {turns}"
        );
        let error = character_error_rate(&truth, &single_spaced(&page.text()));
        assert!(
            error < 0.03,
            "turned {turns}: CER {:.1}%: {:?}",
            error * 100.0,
            page.text()
        );

        // The first word, read upright and mapped onto the turned page, is
        // where the turned page reports it, corners still in reading order.
        let word = &page.lines[0].words[0];
        assert_eq!(word.text, first.text);
        for (got, upright_corner) in word.quad.iter().zip(first.quad) {
            let want = forward(turns, size, (upright_corner[0], upright_corner[1]));
            assert!(
                (got[0] - want.0).abs() < 1e-6 && (got[1] - want.1).abs() < 1e-6,
                "turned {turns}: {:?} vs {want:?}",
                got
            );
        }
    }

    let off = OcrOptions {
        detect_orientation: false,
        ..Default::default()
    };
    assert_eq!(
        engine
            .recognize(&upright.turned(2), &off)
            .orientation_degrees,
        0
    );
}
