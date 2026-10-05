//! Local fidelity checks: cargo run -p pdf_render --example render_png -- input.pdf output.png [scale] [page]
use pdf_core::document::PdfDocument;
use pdf_render::{encode_rgba_as_png, render_page, RenderOptions, RenderSize};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: render_png input.pdf output.png [scale] [page-1-based]".into());
    }
    let doc = PdfDocument::from_bytes(&std::fs::read(&args[1])?)?;
    let scale = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(1.0);
    let page: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(1);
    let rendered = render_page(
        &doc,
        page.checked_sub(1).ok_or("page must start at 1")?,
        RenderOptions {
            size: RenderSize::Scale(scale),
            ..Default::default()
        },
    )?;
    for warning in &rendered.warnings {
        eprintln!("warning: {warning}");
    }
    std::fs::write(
        &args[2],
        encode_rgba_as_png(&rendered.pixels, rendered.width, rendered.height)
            .ok_or("invalid image")?,
    )?;
    eprintln!("Rendered {} x {} pixels", rendered.width, rendered.height);
    Ok(())
}
