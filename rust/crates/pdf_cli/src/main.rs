use std::path::Path;

use anyhow::{bail, Context, Result};
use pdf_core::document::PdfDocument;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("inspect") => {
            let path = args.next().context("usage: pdf_cli inspect <path>")?;
            ensure_no_extra_args(args)?;
            inspect(&path)
        }
        Some("rewrite") => {
            let input = args
                .next()
                .context("usage: pdf_cli rewrite <input> <output>")?;
            let output = args
                .next()
                .context("usage: pdf_cli rewrite <input> <output>")?;
            ensure_no_extra_args(args)?;
            rewrite(&input, &output)
        }
        Some("roundtrip") => {
            let input = args.next().context("usage: pdf_cli roundtrip <input>")?;
            ensure_no_extra_args(args)?;
            roundtrip(&input)
        }
        _ => bail!("usage: pdf_cli <inspect|rewrite|roundtrip> ..."),
    }
}

fn inspect(path: &str) -> Result<()> {
    let doc = PdfDocument::from_path(path).with_context(|| format!("failed to inspect {path}"))?;
    print_inspect(&doc);
    Ok(())
}

fn rewrite(input: &str, output: &str) -> Result<()> {
    let doc = PdfDocument::from_path(input).with_context(|| format!("failed to parse {input}"))?;
    doc.save_as(output)
        .with_context(|| format!("failed to write {output}"))?;
    println!("rewrote {input} -> {output}");
    Ok(())
}

fn roundtrip(input: &str) -> Result<()> {
    let original =
        PdfDocument::from_path(input).with_context(|| format!("failed to parse {input}"))?;
    let output = std::env::temp_dir().join(format!(
        "flutter_pdf_core_roundtrip_{}_{}.pdf",
        std::process::id(),
        Path::new(input)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output")
    ));
    original
        .save_as(&output)
        .with_context(|| format!("failed to write {}", output.display()))?;
    let rewritten = PdfDocument::from_path(&output)
        .with_context(|| format!("failed to parse {}", output.display()))?;

    let original_info = original.inspect();
    let rewritten_info = rewritten.inspect();
    let success = original_info.version == rewritten_info.version
        && original_info.root.is_some() == rewritten_info.root.is_some()
        && original_info.encrypted == rewritten_info.encrypted
        && original_info.page_count == rewritten_info.page_count;

    if success {
        println!("roundtrip: success");
        println!("temp output: {}", output.display());
        Ok(())
    } else {
        println!("roundtrip: failure");
        println!("input:");
        print_inspect(&original);
        println!("rewritten:");
        print_inspect(&rewritten);
        bail!("roundtrip comparison failed");
    }
}

fn print_inspect(doc: &PdfDocument) {
    let info = doc.inspect();
    println!("PDF version: {}", info.version);
    println!("encrypted: {}", if info.encrypted { "yes" } else { "no" });
    println!("object count: {}", info.object_count);
    match info.page_count {
        Some(count) => println!("page count: {count}"),
        None => println!("page count: unresolved"),
    }
    println!("trailer keys: {}", info.trailer_keys.join(", "));
    match info.root {
        Some(root) => println!("root reference: {} {} R", root.number, root.generation),
        None => println!("root reference: unresolved"),
    }
}

fn ensure_no_extra_args(mut args: impl Iterator<Item = String>) -> Result<()> {
    if args.next().is_some() {
        bail!("too many arguments");
    }
    Ok(())
}
