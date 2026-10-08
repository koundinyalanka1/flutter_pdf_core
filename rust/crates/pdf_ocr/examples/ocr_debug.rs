//! Draw what OCR sees on a page: line boxes (red), word boxes (blue) and the
//! recognized text per line, in reading order.
//!
//!     cargo run --release -p pdf_ocr --example ocr_debug -- in.pdf <page-1-based> out.png [dpi]

use pdf_core::document::PdfDocument;
use pdf_ocr::{GrayImage, OcrEngine, OcrOptions};
use pdf_render::{encode_rgba_as_png, render_page, RenderOptions, RenderSize};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let doc = PdfDocument::from_path(&args[0]).expect("open");
    let page: usize = args[1].parse::<usize>().expect("page") - 1;
    let dpi: f64 = args
        .get(3)
        .map(|d| d.parse().expect("dpi"))
        .unwrap_or(300.0);
    let render = render_page(
        &doc,
        page,
        RenderOptions {
            size: RenderSize::Scale(dpi / 72.0),
            ..Default::default()
        },
    )
    .expect("render");
    let (w, h) = (render.width as usize, render.height as usize);
    let image = GrayImage::from_rgba(w, h, &render.pixels).unwrap();
    let ocr = OcrEngine::embedded().recognize(&image, &OcrOptions::default());

    let mut rgba = render.pixels.clone();
    let mut paint = |x: f64, y: f64, color: [u8; 3]| {
        let (x, y) = (x.round() as isize, y.round() as isize);
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            let i = (y as usize * w + x as usize) * 4;
            rgba[i..i + 3].copy_from_slice(&color);
        }
    };
    let mut outline = |b: [f64; 4], color: [u8; 3]| {
        for x in b[0] as usize..=b[2] as usize {
            paint(x as f64, b[1], color);
            paint(x as f64, b[3], color);
        }
        for y in b[1] as usize..=b[3] as usize {
            paint(b[0], y as f64, color);
            paint(b[2], y as f64, color);
        }
    };
    for (i, line) in ocr.lines.iter().enumerate() {
        outline(line.bounds, [230, 0, 0]);
        for word in &line.words {
            outline(word.bounds, [0, 90, 255]);
        }
        println!(
            "{i:3} [{:5.0} {:5.0} {:5.0} {:5.0}] {:.2} {}",
            line.bounds[0],
            line.bounds[1],
            line.bounds[2],
            line.bounds[3],
            line.confidence,
            line.text
        );
    }
    std::fs::write(
        &args[2],
        encode_rgba_as_png(&rgba, w as u32, h as u32).unwrap(),
    )
    .unwrap();
}
