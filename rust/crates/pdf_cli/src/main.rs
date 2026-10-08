//! Developer CLI over the whole library — handy for manual testing on real
//! PDFs without going through Flutter.

use std::time::Instant;

use anyhow::{bail, Context, Result};
use pdf_core::crypt::encrypt_to_bytes;
use pdf_core::document::PdfDocument;
use pdf_ocr::{GrayImage, OcrEngine, PdfOcrOptions};

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
  decrypt   <in> <password> <out>
  ocr        <in> [ranges] [password]          recognize pages (even ones with text)
  searchable <in> <out> [password]             add invisible OCR text to scanned pages
  ocr-eval   <in> [dpi] [password]             OCR rendered pages of a text PDF and
                                               score them against its real text
  ocr-image  <in.pgm>                          recognize a binary (P5) greyscale image";

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
        "ocr" => {
            let doc = open(&rest, 0, 2)?;
            let count = doc.page_count().unwrap_or(0) as usize;
            let pages = match rest.get(1).filter(|r| !r.is_empty()) {
                Some(ranges) => parse_ranges(ranges, count)?,
                None => (0..count).collect(),
            };
            let options = PdfOcrOptions {
                force: true,
                ..Default::default()
            };
            for page in pages {
                let started = Instant::now();
                let ocr = pdf_ocr::pdf::recognize_page(&doc, page, engine(), &options)?;
                println!(
                    "--- page {} ({} words, confidence {:.2}, skew {:.2}°, {:.0} dpi, {:.2}s) ---",
                    page + 1,
                    ocr.word_count(),
                    ocr.mean_confidence(),
                    ocr.skew_degrees,
                    ocr.dpi,
                    started.elapsed().as_secs_f64()
                );
                println!("{}", ocr.text());
            }
        }
        "searchable" => {
            let mut doc = open(&rest, 0, 2)?;
            let pages: Vec<usize> = (0..doc.page_count().unwrap_or(0) as usize).collect();
            let started = Instant::now();
            let reports = pdf_ocr::pdf::make_searchable(
                &mut doc,
                &pages,
                engine(),
                &PdfOcrOptions::default(),
            )?;
            doc.save_as(arg(&rest, 1)?)?;
            for report in &reports {
                println!("page {}: {}", report.page, serde_json::to_string(report)?);
            }
            println!(
                "wrote {} in {:.1}s",
                arg(&rest, 1)?,
                started.elapsed().as_secs_f64()
            );
        }
        "ocr-eval" => {
            let doc = open(&rest, 0, 2)?;
            let dpi = match rest.get(1) {
                Some(dpi) => dpi.parse().context("bad dpi")?,
                None => pdf_ocr::pdf::DEFAULT_DPI,
            };
            let options = PdfOcrOptions {
                dpi,
                force: true,
                ..Default::default()
            };
            let (mut char_errors, mut chars, mut word_errors, mut words) = (0, 0, 0, 0);
            let started = Instant::now();
            for page in 0..doc.page_count().unwrap_or(0) as usize {
                let truth = single_spaced(&pdf_text::extractor::extract_page_text(&doc, page)?);
                if truth.is_empty() {
                    continue;
                }
                let ocr = pdf_ocr::pdf::recognize_page(&doc, page, engine(), &options)?;
                let read = single_spaced(&ocr.text());
                let (truth_chars, read_chars): (Vec<char>, Vec<char>) =
                    (truth.chars().collect(), read.chars().collect());
                let (truth_words, read_words): (Vec<&str>, Vec<&str>) =
                    (truth.split(' ').collect(), read.split(' ').collect());
                let (ce, we) = (
                    edit_distance(&truth_chars, &read_chars),
                    edit_distance(&truth_words, &read_words),
                );
                println!(
                    "page {:3}: CER {:6.2}%  WER {:6.2}%  ({} chars)",
                    page + 1,
                    100.0 * ce as f64 / truth_chars.len() as f64,
                    100.0 * we as f64 / truth_words.len() as f64,
                    truth_chars.len()
                );
                (char_errors, chars, word_errors, words) = (
                    char_errors + ce,
                    chars + truth_chars.len(),
                    word_errors + we,
                    words + truth_words.len(),
                );
            }
            println!(
                "total: CER {:.2}%  WER {:.2}%  over {chars} characters, {:.1}s",
                100.0 * char_errors as f64 / chars.max(1) as f64,
                100.0 * word_errors as f64 / words.max(1) as f64,
                started.elapsed().as_secs_f64()
            );
        }
        "ocr-image" => {
            let bytes = std::fs::read(arg(&rest, 0)?)?;
            let image = read_pgm(&bytes).context("not a binary (P5) 8-bit PGM image")?;
            let started = Instant::now();
            let page = engine().recognize(&image, &Default::default());
            eprintln!(
                "{} lines, skew {:.2}°, {:.2}s",
                page.lines.len(),
                page.skew_degrees,
                started.elapsed().as_secs_f64()
            );
            println!("{}", page.text());
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

/// The compiled-in OCR model, or the model file named by `PDF_OCR_MODEL`
/// (for evaluating a newly trained model without rebuilding).
fn engine() -> &'static OcrEngine {
    static ENGINE: std::sync::OnceLock<&'static OcrEngine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| match std::env::var_os("PDF_OCR_MODEL") {
        Some(path) => {
            let bytes = std::fs::read(&path).expect("PDF_OCR_MODEL is readable");
            Box::leak(Box::new(
                OcrEngine::from_model(&bytes).expect("PDF_OCR_MODEL is a valid model"),
            ))
        }
        None => OcrEngine::embedded(),
    })
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

/// Whitespace collapsed to single spaces: line breaks are layout, not text,
/// when scoring recognition.
fn single_spaced(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn edit_distance<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(x != y)),
            );
        }
        previous = current;
    }
    previous[b.len()]
}

/// A binary greyscale PGM ("P5", maxval 255), as image tools write them.
fn read_pgm(bytes: &[u8]) -> Option<GrayImage> {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'#') {
            if bytes[at] == b'#' {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            at += 1;
        }
        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(std::str::from_utf8(&bytes[start..at]).ok()?);
    }
    let (width, height): (usize, usize) = (fields[1].parse().ok()?, fields[2].parse().ok()?);
    if fields[0] != "P5" || fields[3] != "255" {
        return None;
    }
    let pixels = bytes
        .get(at + 1..at + 1 + width.checked_mul(height)?)?
        .to_vec();
    GrayImage::new(width, height, pixels)
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
