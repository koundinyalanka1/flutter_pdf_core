//! Render a range of pages small and tile them into one PNG, so a whole
//! document can be eyeballed at once after a renderer change.
//!
//! cargo run --release -p pdf_render --example contact_sheet -- <in.pdf> <out.png> [first] [last] [columns]

use pdf_core::document::PdfDocument;
use pdf_render::{encode_rgba_as_png, render_page, RenderOptions, RenderSize};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: contact_sheet <pdf> <out.png> [first] [last] [cols]");
    let out = args.next().expect("out.png");
    let doc = PdfDocument::from_path_with_password(&path, "").unwrap();
    let count = doc.page_count().unwrap_or(0) as usize;
    let first: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(1);
    let last: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(count);
    let cols: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(6);
    let (cell_w, cell_h) = (220u32, 300u32);

    let pages: Vec<usize> = (first..=last.min(count)).collect();
    let rows = pages.len().div_ceil(cols);
    let (sheet_w, sheet_h) = (cell_w as usize * cols, cell_h as usize * rows);
    let mut sheet = vec![200u8; sheet_w * sheet_h * 4];

    for (i, &page) in pages.iter().enumerate() {
        let options = RenderOptions {
            size: RenderSize::FitBox {
                width: cell_w - 6,
                height: cell_h - 6,
            },
            ..Default::default()
        };
        let Ok(rendered) = render_page(&doc, page - 1, options) else {
            eprintln!("page {page}: render failed");
            continue;
        };
        let (ox, oy) = (
            (i % cols) * cell_w as usize + 3,
            (i / cols) * cell_h as usize + 3,
        );
        for y in 0..rendered.height as usize {
            for x in 0..rendered.width as usize {
                let src = (y * rendered.width as usize + x) * 4;
                let dst = ((oy + y) * sheet_w + ox + x) * 4;
                sheet[dst..dst + 4].copy_from_slice(&rendered.pixels[src..src + 4]);
            }
        }
    }
    let png = encode_rgba_as_png(&sheet, sheet_w as u32, sheet_h as u32).unwrap();
    std::fs::write(out, png).unwrap();
}
