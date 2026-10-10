//! Regenerates `data/cmaps.bin`, the predefined CMaps compiled into
//! `pdf_text::cmap`.
//!
//! ```text
//! cargo run --release -p pdf_text --example build_cmap_bundle -- \
//!     path/to/cmap-resources path/to/mapping-resources-pdf crates/pdf_text/data/cmaps.bin
//! ```
//!
//! Sources: <https://github.com/adobe-type-tools/cmap-resources> (every
//! `Adobe-*/CMap/*`) and <https://github.com/adobe-type-tools/mapping-resources-pdf>
//! (`pdf2unicode/Adobe-*-UCS2`), both BSD-3-Clause. The files are read with
//! the same parser the library uses for CMaps embedded in documents.

use std::path::{Path, PathBuf};

use pdf_text::cmap::{encode_bundle, parse, ParsedCMap};

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: build_cmap_bundle <cmap-resources> <mapping-resources-pdf> <out>";
    let cmaps = PathBuf::from(args.next().expect(usage));
    let mappings = PathBuf::from(args.next().expect(usage));
    let out = PathBuf::from(args.next().expect(usage));

    let mut entries = Vec::new();
    for collection in sorted_dir(&cmaps) {
        let name = collection
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if !name.starts_with("Adobe-") {
            continue;
        }
        for file in sorted_dir(&collection.join("CMap")) {
            entries.push(load(&file));
        }
    }
    for file in sorted_dir(&mappings.join("pdf2unicode")) {
        entries.push(load(&file));
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name);

    let bundle = encode_bundle(&entries);
    std::fs::write(&out, &bundle).expect("write bundle");
    let ranges: usize = entries.iter().map(|e| e.cid_ranges.len()).sum();
    let unicode: usize = entries.iter().map(|e| e.unicode_ranges.len()).sum();
    eprintln!(
        "{} CMaps, {ranges} CID ranges, {unicode} Unicode ranges -> {} ({} bytes)",
        entries.len(),
        out.display(),
        bundle.len()
    );
}

fn load(file: &Path) -> ParsedCMap {
    let data = std::fs::read(file).expect("read CMap");
    let mut parsed = parse(&data);
    if parsed.name.is_empty() {
        parsed.name = file.file_name().unwrap().to_string_lossy().into_owned();
    }
    parsed
}

fn sorted_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    paths.sort();
    paths
}
