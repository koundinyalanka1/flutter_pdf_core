#[path = "../../../tests/support/testkit.rs"]
mod testkit;

use std::time::Duration;

use pdf_core::document::PdfDocument;
use pdf_core::filter::{decode, decode_with_dict, flate_encode, DecodeParms};
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_edit::{Annotation, Color, Edits, Kind};
use pdf_render::font::{cff::CffFont, truetype::TrueTypeFont, type1::Type1Font};
use pdf_render::image::{decode_image_within, ImageBudget};
use testkit::{mutate, run_limited, Failure, Limits, Rng};

const SEED: u64 = 0x5044_465F_4239_0001;
const SIMPLE: &[u8] = include_bytes!("../../../fixtures/simple.pdf");
const TWO_PAGES: &[u8] = include_bytes!("../../../fixtures/two_pages.pdf");
const FONT: &[u8] = include_bytes!("../../pdf_render/fonts/Roboto-Regular.ttf");
const PALETTE: &[u8] = include_bytes!("../../pdf_render/tests/fixtures/codecs/indexed-small.jp2");
const RGBA: &[u8] = include_bytes!("../../pdf_render/tests/fixtures/codecs/rgba-u4.jp2");
const SYMBOLS: &[u8] = include_bytes!("../../pdf_render/tests/fixtures/codecs/bitmap-symbol-global.jbig2");

fn object_stream() -> Vec<u8> {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    let mut offsets = [0u32; 5];
    for (number, body) in [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[4 0 R]/Count 1>>"),
        (4, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 20 20]>>"),
    ] {
        offsets[number] = pdf.len() as u32;
        pdf.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    let data = flate_encode(b"10 0 (stored)");
    offsets[3] = pdf.len() as u32;
    pdf.extend_from_slice(format!("3 0 obj\n<</Type/ObjStm/N 1/First 5/Filter/FlateDecode/Length {}>>\nstream\n", data.len()).as_bytes());
    pdf.extend_from_slice(&data);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let mut rows = vec![0; 7];
    for offset in &offsets[1..] {
        rows.push(1);
        rows.extend_from_slice(&offset.to_be_bytes());
        rows.extend_from_slice(&[0, 0]);
    }
    rows.extend_from_slice(&[2, 0, 0, 0, 3, 0, 0]);
    let xref = pdf.len();
    let rows = flate_encode(&rows);
    pdf.extend_from_slice(format!("7 0 obj\n<</Type/XRef/Size 11/W[1 4 2]/Index[0 5 10 1]/Root 1 0 R/Filter/FlateDecode/Length {}>>\nstream\n", rows.len()).as_bytes());
    pdf.extend_from_slice(&rows);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    pdf
}

#[test]
fn fuzz_xref_recovery_and_text_fixed_seed() {
    run_limited("fuzz_xref_recovery_and_text_fixed_seed", Limits::mutation(Duration::from_secs(90)), || {
        let stream = object_stream();
        assert_eq!(PdfDocument::from_bytes(&stream).unwrap().page_count(), Some(1));
        let mut recovery = SIMPLE.to_vec();
        if let Some(at) = recovery.windows(9).position(|w| w == b"startxref") {
            recovery[at..at + 9].copy_from_slice(b"brokenref");
        }
        let seeds = [SIMPLE, TWO_PAGES, stream.as_slice(), recovery.as_slice()];
        let mut rng = Rng::new(SEED);
        let mut opened = 0;
        for iteration in 0..4096 {
            let bytes = mutate(&mut rng, seeds[iteration % seeds.len()], seeds[(iteration + 1) % seeds.len()]);
            if let Ok(doc) = PdfDocument::from_bytes(&bytes) {
                opened += 1;
                let _ = doc.collect_page_ids();
                let _ = pdf_text::extractor::extract_page_text(&doc, 0);
            }
        }
        assert!(opened > 0);
        eprintln!("PDF xref/recovery/text seed={SEED:x} iterations=4096 opened={opened}");
    }).unwrap();
}

#[test]
fn fuzz_filter_chains_and_predictors_fixed_seed() {
    run_limited("fuzz_filter_chains_and_predictors_fixed_seed", Limits::mutation(Duration::from_secs(90)), || {
        let compressed = flate_encode(b"\0abcdef\0ghijkl");
        let hex = format!("{}>", compressed.iter().map(|b| format!("{b:02x}")).collect::<String>()).into_bytes();
        let seeds = [compressed.as_slice(), hex.as_slice(), b"z~>", &[128, 0, 128], &[0x80, 0, 0x20, 0x20]];
        let mut rng = Rng::new(SEED ^ 1);
        for iteration in 0..4096 {
            let bytes = mutate(&mut rng, seeds[iteration % seeds.len()], seeds[(iteration + 1) % seeds.len()]);
            let parms = DecodeParms { predictor: [1, 2, 10, 15][iteration % 4], colors: 1 + iteration % 4, columns: [1, 2, 16, 32767][iteration % 4], ..DecodeParms::default() };
            for filter in ["FlateDecode", "LZWDecode", "RunLengthDecode", "ASCIIHexDecode", "ASCII85Decode"] {
                let _ = decode(filter, &bytes, &parms);
            }
            let mut dictionary = Dictionary::new();
            dictionary.insert("Filter".into(), PdfObject::Array(vec![PdfObject::Name("ASCIIHexDecode".into()), PdfObject::Name("FlateDecode".into())]));
            let _ = decode_with_dict(&dictionary, &bytes);
        }
        eprintln!("PDF filters seed={:x} iterations=4096 filters=5+chain", SEED ^ 1);
    }).unwrap();
}

#[test]
fn fuzz_embedded_font_parsers_fixed_seed() {
    run_limited("fuzz_embedded_font_parsers_fixed_seed", Limits::mutation(Duration::from_secs(90)), || {
        let cff = [1, 0, 4, 4, 0, 1, 1, 1, 2, b'X', 0, 1, 1, 1, 3, 160, 17, 0, 0, 0, 0, 0, 1, 1, 1, 2, 14];
        assert!(CffFont::parse(cff.to_vec()).is_some());
        assert!(TrueTypeFont::parse(FONT.to_vec()).is_some());
        let seeds = [FONT, cff.as_slice(), b"%!PS-AdobeFont-1.0: Test\ncurrentfile eexec\n00000000".as_slice()];
        let mut rng = Rng::new(SEED ^ 2);
        for iteration in 0..2048 {
            let bytes = mutate(&mut rng, seeds[iteration % seeds.len()], seeds[(iteration + 1) % seeds.len()]);
            if let Some(font) = TrueTypeFont::parse(bytes.clone()) {
                for character in [0, 32, 65, 0x20ac, 0xffff] {
                    if let Some(glyph) = font.glyph_for_char(character) { let _ = font.glyph_outline(glyph); let _ = font.advance(glyph); }
                }
            }
            if let Some(font) = CffFont::parse(bytes.clone()) {
                for glyph in 0..font.num_glyphs().min(16) { let _ = font.glyph_outline(glyph as u16); }
            }
            if let Some(font) = Type1Font::parse(&bytes, None) { let _ = font.glyph_outline("A"); }
        }
        eprintln!("PDF fonts seed={:x} iterations=2048 (valid TrueType/CFF seeds; Type1 rejection paths)", SEED ^ 2);
    }).unwrap();
}

fn image_stream(filter: &str, bytes: Vec<u8>) -> PdfStream {
    let mut dict = Dictionary::new();
    dict.insert("Width".into(), PdfObject::Integer(2));
    dict.insert("Height".into(), PdfObject::Integer(2));
    dict.insert("Filter".into(), PdfObject::Name(filter.into()));
    PdfStream::new(dict, bytes)
}

#[test]
fn fuzz_jpeg_jpx_jbig2_fixed_seed() {
    run_limited("fuzz_jpeg_jpx_jbig2_fixed_seed", Limits::mutation(Duration::from_secs(120)), || {
        let jpeg = pdf_render::encode_rgba_as_jpeg(&[255; 16], 2, 2, 80).unwrap();
        let seeds = [("DCTDecode", jpeg.as_slice()), ("JPXDecode", PALETTE), ("JPXDecode", RGBA), ("JBIG2Decode", &SYMBOLS[300..])];
        let mut rng = Rng::new(SEED ^ 3);
        for (index, (filter, seed)) in seeds.iter().enumerate() {
            let mut decoded = 0;
            for iteration in 0..512 {
                let mut doc = PdfDocument::new_empty("1.7");
                let bytes = if iteration == 0 { seed.to_vec() } else { mutate(&mut rng, seed, seeds[(index + 1) % seeds.len()].1) };
                let mut stream = image_stream(filter, bytes);
                if *filter == "JBIG2Decode" {
                    let globals = if iteration % 5 == 0 { SYMBOLS[13..300].to_vec() } else { mutate(&mut rng, &SYMBOLS[13..300], seed) };
                    let id = doc.add_object(PdfObject::Stream(PdfStream::new(Dictionary::new(), globals)));
                    let mut parms = Dictionary::new();
                    parms.insert("JBIG2Globals".into(), PdfObject::Reference(id));
                    stream.dictionary.insert("DecodeParms".into(), PdfObject::Dictionary(parms));
                }
                let image = decode_image_within(&doc, &stream, &ImageBudget::new(64 << 20));
                if iteration == 0 { assert!(image.is_some(), "seed {filter}"); }
                decoded += usize::from(image.is_some());
            }
            eprintln!("PDF codec={filter} seed={:x} iterations=512 decoded={decoded}", SEED ^ 3);
        }
    }).unwrap();
}

#[test]
fn no_op_is_identical_and_incremental_edit_preserves_original_bytes_and_content() {
    run_limited("no_op_is_identical_and_incremental_edit_preserves_original_bytes_and_content", Limits::mutation(Duration::from_secs(15)), || {
        let original = PdfDocument::from_bytes(SIMPLE).unwrap();
        let original_text = pdf_text::extractor::extract_page_text(&original, 0).unwrap();
        let (unchanged, _) = pdf_edit::apply(SIMPLE.to_vec(), "", &Edits::default()).unwrap();
        assert_eq!(unchanged, SIMPLE);
        let annotation = Annotation {
            page: 1, kind: Kind::Note { at: [12.0, 12.0], text: "Phase10 saved content".into() },
            color: Color::BLACK, fill: None, width: 1.0, opacity: 1.0,
            contents: "Phase10 saved content".into(), author: "test".into(), flatten: false,
        };
        let edits = Edits { annotations: vec![annotation], ..Edits::default() };
        let (saved, _) = pdf_edit::apply(SIMPLE.to_vec(), "", &edits).unwrap();
        assert!(saved.starts_with(SIMPLE));
        let reopened = PdfDocument::from_bytes(&saved).unwrap();
        assert_eq!(reopened.page_count(), original.page_count());
        assert_eq!(pdf_text::extractor::extract_page_text(&reopened, 0).unwrap(), original_text);
        let annotations = pdf_edit::annotations(&reopened, 0).unwrap();
        assert!(annotations.iter().any(|a| a.contents == "Phase10 saved content"));
    }).unwrap();
}

#[test]
fn supervisor_timeout_is_a_failure() {
    let result = run_limited("supervisor_timeout_is_a_failure", Limits::new(Duration::from_millis(250)), || {
        loop { std::thread::sleep(Duration::from_secs(1)); }
    });
    assert!(matches!(result, Err(Failure::Timeout(_))), "{result:?}");
}

#[test]
fn supervisor_panic_is_distinct() {
    let result = run_limited("supervisor_panic_is_distinct", Limits::new(Duration::from_secs(5)), || panic!("intentional worker panic"));
    assert!(matches!(result, Err(Failure::Panic)), "{result:?}");
}
