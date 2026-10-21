use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use pdf_core::document::PdfDocument;
use pdf_core::error::PdfError;
use pdf_edit::{Annotation, Color, Edits, Kind};
use pdf_render::{render_page, RenderOptions, RenderSize};
use serde_json::{json, Value};

const MARKER: &str = "Phase10 saved content";

fn hash(bytes: &[u8]) -> String {
    let value = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3));
    format!("{value:016x}")
}

fn execute(args: Vec<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let operation = args.get(1).ok_or("operation required: inspect, render, text, edit, save or export")?;
    let path = Path::new(args.get(2).ok_or("input file required")?);
    let page = args.get(3).map(|s| s.parse::<usize>()).transpose()?.unwrap_or(0);
    let password = args.get(4).map(String::as_str).unwrap_or("");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take((512 << 20) + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 512 << 20 { return Err("TOO_LARGE: input exceeds 512 MiB".into()); }
    let doc = PdfDocument::from_bytes_with_password(&bytes, password)?;
    let count = doc.page_count().ok_or("CORRUPT: no page tree")?;
    if count == 0 { return Err("CORRUPT: zero pages".into()); }
    match operation.as_str() {
        "inspect" => Ok(json!({"pages": count, "recovered": !doc.recovery_warnings.is_empty()})),
        "render" => {
            let options = RenderOptions {
                size: RenderSize::FitBox { width: 400, height: 400 },
                max_pixels: 160_000, threads: 1, time_limit: Some(Duration::from_secs(20)),
                ..RenderOptions::default()
            };
            let image = render_page(&doc, page, options)?;
            if image.width == 0 || image.height == 0 || image.pixels.len() != image.width as usize * image.height as usize * 4 {
                return Err("CORRUPT: incomplete rendered pixels".into());
            }
            Ok(json!({"width": image.width, "height": image.height, "pixel_hash": hash(&image.pixels), "warnings": image.warnings}))
        }
        "text" => {
            let text = pdf_text::extractor::extract_page_text(&doc, page)?;
            Ok(json!({"text_hash": hash(text.as_bytes()), "characters": text.chars().count()}))
        }
        "edit" | "save" | "export" => {
            let output = Path::new(args.get(5).ok_or("output file required")?);
            if output.exists() || path == output { return Err("INVALID_ARGUMENT: output must be a new file".into()); }
            if operation == "export" {
                doc.save_as(output)?;
            } else {
                let edits = if operation == "edit" {
                    Edits { annotations: vec![Annotation {
                        page: 1, kind: Kind::Note { at: [12.0, 12.0], text: MARKER.into() },
                        color: Color::BLACK, fill: None, width: 1.0, opacity: 1.0,
                        contents: MARKER.into(), author: "test".into(), flatten: false,
                    }], ..Edits::default() }
                } else { Edits::default() };
                let (saved, warnings) = pdf_edit::apply(bytes.clone(), password, &edits)?;
                if operation == "save" && saved != bytes { return Err("CONTENT_CHANGED: unedited save is not byte-identical".into()); }
                if operation == "edit" && doc.recovery_warnings.is_empty() && !saved.starts_with(&bytes) {
                    return Err("CONTENT_CHANGED: incremental save changed the original prefix".into());
                }
                let reopened = PdfDocument::from_bytes_with_password(&saved, password)?;
                if reopened.page_count() != Some(count) { return Err("CONTENT_CHANGED: page count changed".into()); }
                if operation == "edit" && !pdf_edit::annotations(&reopened, 0)?.iter().any(|a| a.contents == MARKER) {
                    return Err("CONTENT_CHANGED: saved annotation is missing".into());
                }
                let mut file = std::fs::File::create_new(output)?;
                file.write_all(&saved)?;
                file.sync_all()?;
                return Ok(json!({"pages": count, "warnings": warnings, "prefix_checked": doc.recovery_warnings.is_empty()}));
            }
            let saved = std::fs::read(output)?;
            let reopened = PdfDocument::from_bytes(&saved)?;
            if reopened.page_count() != Some(count) { return Err("CONTENT_CHANGED: exported page count changed".into()); }
            Ok(json!({"pages": count}))
        }
        _ => Err("INVALID_ARGUMENT: unknown operation".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let task = std::thread::Builder::new().stack_size(512 * 1024).spawn(move || execute(args));
    let result = match task {
        Ok(task) => task.join(),
        Err(error) => { eprintln!("UNSUPPORTED: {error}"); std::process::exit(83); }
    };
    match result {
        Ok(Ok(value)) => println!("{value}"),
        Ok(Err(error)) => {
            let code = if let Some(pdf) = error.downcast_ref::<PdfError>() {
                match pdf {
                    PdfError::Encrypted => "ENCRYPTED",
                    PdfError::WrongPassword => "WRONG_PASSWORD",
                    PdfError::UnsupportedFilter(_) | PdfError::Unsupported(_) => "UNSUPPORTED",
                    _ => "CORRUPT",
                }
            } else { "ERROR" };
            println!("{}", json!({"error": code, "message": error.to_string()}));
            std::process::exit(if code == "UNSUPPORTED" { 83 } else { 2 });
        }
        Err(_) => { eprintln!("PANIC: worker panicked"); std::process::exit(81); }
    }
}
