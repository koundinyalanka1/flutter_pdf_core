//! Developer CLI over the whole library — handy for manual testing on real
//! PDFs without going through Flutter.

use anyhow::{bail, Context, Result};
use pdf_core::crypt::encrypt_to_bytes;
use pdf_core::document::PdfDocument;

const USAGE: &str = "usage: pdf_cli <command> ...

  inspect   <in> [password]                    summary JSON
  rewrite   <in> <out> [password]              parse + clean rewrite
  roundtrip <in> [password]                    parse, write, re-parse check
  text      <in> [page-1-based] [password]     extract text
  split     <in> <ranges> <out> [password]     e.g. ranges \"1-3,5\"
  delete    <in> <ranges> <out> [password]
  reorder   <in> <order> <out> [password]      e.g. order \"3,1,2\"
  merge     <out> <in1> <in2> [in3 ...]
  rotate    <in> <degrees> <out> [password]    rotates all pages
  meta-get  <in> [password]
  meta-set  <in> <json> <out> [password]
  export    <in> [json|ndjson] [password]      AI-ready export to stdout
  encrypt   <in> <user-pw> <owner-pw> <out>
  decrypt   <in> <password> <out>";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter().map(String::as_str);
    let command = it.next().unwrap_or("");
    let rest: Vec<&str> = it.collect();

    match command {
        "inspect" => {
            let doc = open(&rest, 0, 1)?;
            print_inspect(&doc);
        }
        "rewrite" => {
            let doc = open(&rest, 0, 2)?;
            doc.save_as(arg(&rest, 1)?)?;
            println!("wrote {}", arg(&rest, 1)?);
        }
        "roundtrip" => {
            let doc = open(&rest, 0, 1)?;
            let bytes = doc.to_bytes()?;
            let reparsed = PdfDocument::from_bytes(&bytes).context("re-parse failed")?;
            println!(
                "ok: {} objects, {} pages",
                reparsed.objects.len(),
                reparsed.page_count().unwrap_or(0)
            );
        }
        "text" => {
            let doc = open_with_pw(arg(&rest, 0)?, rest.get(2).copied().unwrap_or(""))?;
            match rest.get(1).and_then(|p| p.parse::<usize>().ok()) {
                Some(page) if page >= 1 => {
                    println!("{}", pdf_text::extractor::extract_page_text(&doc, page - 1)?)
                }
                _ => {
                    for (i, text) in pdf_text::extractor::extract_all_pages(&doc)?
                        .iter()
                        .enumerate()
                    {
                        println!("--- page {} ---", i + 1);
                        println!("{text}");
                    }
                }
            }
        }
        "split" => {
            let doc = open(&rest, 0, 3)?;
            let count = doc.page_count().unwrap_or(0) as usize;
            let indices = parse_ranges(arg(&rest, 1)?, count)?;
            pdf_ops::split::extract_pages(&doc, &indices)?.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        "delete" => {
            let mut doc = open(&rest, 0, 3)?;
            let count = doc.page_count().unwrap_or(0) as usize;
            let indices = parse_ranges(arg(&rest, 1)?, count)?;
            pdf_ops::split::delete_pages(&mut doc, &indices)?;
            doc.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        "reorder" => {
            let mut doc = open(&rest, 0, 3)?;
            let count = doc.page_count().unwrap_or(0) as usize;
            let order = parse_ranges(arg(&rest, 1)?, count)?;
            pdf_ops::split::reorder_pages(&mut doc, &order)?;
            doc.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        "merge" => {
            if rest.len() < 3 {
                bail!("{USAGE}");
            }
            let merged = pdf_ops::merge::merge_files(&rest[1..])?;
            merged.save_as(rest[0])?;
            println!(
                "wrote {} ({} pages)",
                rest[0],
                merged.page_count().unwrap_or(0)
            );
        }
        "rotate" => {
            let mut doc = open_with_pw(arg(&rest, 0)?, rest.get(3).copied().unwrap_or(""))?;
            let degrees: i64 = arg(&rest, 1)?.parse().context("bad degrees")?;
            pdf_ops::rotate::rotate_all_pages(&mut doc, degrees)?;
            doc.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        "meta-get" => {
            let doc = open(&rest, 0, 1)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&pdf_ops::metadata::read_metadata(&doc))?
            );
        }
        "meta-set" => {
            let mut doc = open_with_pw(arg(&rest, 0)?, rest.get(3).copied().unwrap_or(""))?;
            let meta: pdf_ops::metadata::DocumentMetadata =
                serde_json::from_str(arg(&rest, 1)?).context("bad metadata JSON")?;
            pdf_ops::metadata::write_metadata(&mut doc, &meta)?;
            doc.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        "export" => {
            let doc = open_with_pw(arg(&rest, 0)?, rest.get(2).copied().unwrap_or(""))?;
            let options = pdf_ai::chunker::ChunkOptions::default();
            let output = match rest.get(1).copied().unwrap_or("json") {
                "ndjson" => pdf_ai::export::to_ndjson(&doc, options)?,
                _ => pdf_ai::export::to_json(&doc, options)?,
            };
            println!("{output}");
        }
        "encrypt" => {
            let doc = open_with_pw(arg(&rest, 0)?, "")?;
            let bytes = encrypt_to_bytes(&doc, arg(&rest, 1)?, arg(&rest, 2)?)?;
            std::fs::write(arg(&rest, 3)?, bytes)?;
            println!("wrote {}", arg(&rest, 3)?);
        }
        "decrypt" => {
            let doc = open_with_pw(arg(&rest, 0)?, arg(&rest, 1)?)?;
            doc.save_as(arg(&rest, 2)?)?;
            println!("wrote {}", arg(&rest, 2)?);
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn arg<'a>(rest: &[&'a str], index: usize) -> Result<&'a str> {
    rest.get(index).copied().with_context(|| USAGE.to_string())
}

/// Open `rest[path_index]`, treating the argument at `pw_index` (if present)
/// as an optional password.
fn open(rest: &[&str], path_index: usize, pw_index: usize) -> Result<PdfDocument> {
    let path = arg(rest, path_index)?;
    let password = rest.get(pw_index).copied().unwrap_or("");
    open_with_pw(path, password)
}

fn open_with_pw(path: &str, password: &str) -> Result<PdfDocument> {
    PdfDocument::from_path_with_password(path, password)
        .with_context(|| format!("failed to open {path}"))
}

fn parse_ranges(spec: &str, page_count: usize) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let lo: usize = lo.parse().with_context(|| format!("bad range '{part}'"))?;
        let hi: usize = hi.parse().with_context(|| format!("bad range '{part}'"))?;
        if lo == 0 || hi < lo || hi > page_count {
            bail!("range '{part}' out of bounds (document has {page_count} pages)");
        }
        out.extend((lo - 1)..hi);
    }
    if out.is_empty() {
        bail!("empty page selection");
    }
    Ok(out)
}

fn print_inspect(doc: &PdfDocument) {
    let info = doc.inspect();
    let metadata = pdf_ops::metadata::read_metadata(doc);
    let value = serde_json::json!({
        "version": info.version,
        "encrypted": info.encrypted,
        "objectCount": info.object_count,
        "pageCount": info.page_count,
        "trailerKeys": info.trailer_keys,
        "metadata": metadata,
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
