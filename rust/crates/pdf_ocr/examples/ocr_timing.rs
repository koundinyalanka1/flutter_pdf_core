//! Time OCR on one page: rendering, then recognition with and without
//! orientation detection, and on a single thread.
//!
//!     cargo run --release -p pdf_ocr --example ocr_timing -- in.pdf <page-1-based>

use std::time::Instant;

use pdf_core::document::PdfDocument;
use pdf_ocr::{GrayImage, OcrEngine, OcrOptions};
use pdf_render::{render_page, RenderOptions, RenderSize};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let doc = PdfDocument::from_path(&args[0]).expect("open");
    let page: usize = args[1].parse::<usize>().expect("page") - 1;
    let started = Instant::now();
    let options = RenderOptions {
        size: RenderSize::Scale(300.0 / 72.0),
        ..Default::default()
    };
    let render = render_page(&doc, page, options).expect("render");
    let image = GrayImage::from_rgba(
        render.width as usize,
        render.height as usize,
        &render.pixels,
    )
    .expect("grey");
    println!(
        "render + grey       {:6.0} ms",
        started.elapsed().as_secs_f64() * 1e3
    );
    let engine = OcrEngine::embedded();
    for (name, options) in [
        ("recognize           ", OcrOptions::default()),
        (
            "without orientation ",
            OcrOptions {
                detect_orientation: false,
                ..Default::default()
            },
        ),
        (
            "one thread          ",
            OcrOptions {
                threads: 1,
                ..Default::default()
            },
        ),
    ] {
        let started = Instant::now();
        let ocr = engine.recognize(&image, &options);
        println!(
            "{name}{:6.0} ms  ({} lines)",
            started.elapsed().as_secs_f64() * 1e3,
            ocr.lines.len()
        );
    }
}
