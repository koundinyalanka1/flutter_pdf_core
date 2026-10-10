//! Regenerates `data/fallback-fonts.bin`, the substitute faces compiled into
//! `pdf_render::font::fallback` for scripts Roboto does not cover.
//!
//! ```text
//! cargo run --release -p pdf_render --example build_fallback_bundle -- \
//!     <dir of .ttf files> crates/pdf_render/data/fallback-fonts.bin
//! ```
//!
//! The faces are Noto (SIL Open Font License 1.1, see
//! `data/LICENSE-noto-fonts.txt`), downloaded unhinted from
//! <https://github.com/notofonts/notofonts.github.io/tree/main/fonts> as
//! `<Family>/unhinted/ttf/<Family>-Regular.ttf` for each family listed in
//! `data/fallback-fonts.txt`. Each is stored deflated with its coverage
//! precomputed, so the renderer knows which face to open without opening any.

use std::path::PathBuf;

use pdf_render::font::system::cmap_ranges;
use pdf_render::font::truetype::TrueTypeFont;

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: build_fallback_bundle <font dir> <out>";
    let dir = PathBuf::from(args.next().expect(usage));
    let out = PathBuf::from(args.next().expect(usage));

    let list = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/fallback-fonts.txt"),
    )
    .expect("family list");
    let families: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let mut faces = Vec::new();
    for family in &families {
        let path = dir.join(format!("{family}-Regular.ttf"));
        let data = std::fs::read(&path).unwrap_or_else(|_| panic!("missing {}", path.display()));
        let font = TrueTypeFont::parse(data.clone()).expect("parses");
        assert!(font.has_outlines(), "{family} has no outlines");
        let cmap = cmap_table(&data).expect("cmap");
        let coverage = cmap_ranges(&cmap);
        let serif = family.contains("Serif") || family.contains("Naskh");
        faces.push(pdf_render::font::fallback::BundledFace {
            name: family.to_string(),
            serif,
            coverage,
            data,
        });
    }
    let bundle = pdf_render::font::fallback::encode_bundle(&faces);
    std::fs::write(&out, &bundle).expect("write");
    let raw: usize = faces.iter().map(|f| f.data.len()).sum();
    eprintln!(
        "{} faces, {} bytes raw -> {} bytes bundled at {}",
        faces.len(),
        raw,
        bundle.len(),
        out.display()
    );
}

/// The raw `cmap` table of a single-face font.
fn cmap_table(data: &[u8]) -> Option<Vec<u8>> {
    let count = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..count {
        let record = 12 + i * 16;
        if &data[record..record + 4] == b"cmap" {
            let offset =
                u32::from_be_bytes(data[record + 8..record + 12].try_into().ok()?) as usize;
            let length =
                u32::from_be_bytes(data[record + 12..record + 16].try_into().ok()?) as usize;
            return data.get(offset..offset + length).map(<[u8]>::to_vec);
        }
    }
    None
}
