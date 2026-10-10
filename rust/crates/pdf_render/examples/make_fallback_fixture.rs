//! Write a one-page PDF whose text is in fonts it does not embed — the
//! standard 14, Symbol and ZapfDingbats, a ToUnicode-only CID font across
//! many scripts, and Japanese, Chinese and Korean through predefined CMaps —
//! so font substitution can be checked by eye. CJK draws from installed
//! fonts, so it depends on the machine.
//!
//! cargo run --release -p pdf_render --example make_fallback_fixture -- <out.pdf>

use pdf_core::document::PdfDocument;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;

fn name(n: &str) -> PdfObject {
    PdfObject::Name(n.into())
}

fn dict(entries: Vec<(&str, PdfObject)>) -> Dictionary {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn ucs2(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

fn main() {
    let out = std::env::args().nth(1).expect("out.pdf");
    let mut doc = PdfDocument::new_empty("1.7");
    let mut fonts = Dictionary::new();
    let mut add = |doc: &mut PdfDocument, key: &str, font: Dictionary| {
        let id = doc.add_object(PdfObject::Dictionary(font));
        fonts.insert(key.to_owned(), PdfObject::Reference(id));
    };

    // The standard 14, unembedded, without /Widths.
    for (key, base) in [
        ("H", "Helvetica"),
        ("HB", "Helvetica-Bold"),
        ("T", "Times-Roman"),
        ("TI", "Times-Italic"),
        ("C", "Courier"),
        ("S", "Symbol"),
        ("Z", "ZapfDingbats"),
    ] {
        add(
            &mut doc,
            key,
            dict(vec![
                ("Type", name("Font")),
                ("Subtype", name("Type1")),
                ("BaseFont", name(base)),
            ]),
        );
    }

    // An unembedded CID font whose only key to its glyphs is a ToUnicode
    // CMap: code n is the n-th character of `scripts`.
    let scripts = "తెలుగు हिन्दी عربي עברית ไทย አማርኛ ქართული";
    let chars: Vec<char> = scripts.chars().collect();
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n1 begincodespacerange <0000> <FFFF> endcodespacerange\n",
    );
    cmap.push_str(&format!("{} beginbfchar\n", chars.len()));
    for (i, ch) in chars.iter().enumerate() {
        cmap.push_str(&format!(
            "<{:04X}> <{}>\n",
            i + 1,
            hex(&ucs2(&ch.to_string()))
        ));
    }
    cmap.push_str("endbfchar\nendcmap CMapName currentdict /CMap defineresource pop end end\n");
    let to_unicode = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        cmap.into_bytes(),
    )));
    let world = |doc: &mut PdfDocument, ordering: &str, base: &str| {
        PdfObject::Reference(doc.add_object(PdfObject::Dictionary(dict(vec![
            ("Type", name("Font")),
            ("Subtype", name("CIDFontType2")),
            ("BaseFont", name(base)),
            (
                "CIDSystemInfo",
                PdfObject::Dictionary(dict(vec![
                    ("Registry", PdfObject::LiteralString(b"Adobe".to_vec())),
                    (
                        "Ordering",
                        PdfObject::LiteralString(ordering.as_bytes().to_vec()),
                    ),
                    ("Supplement", PdfObject::Integer(0)),
                ])),
            ),
            ("DW", PdfObject::Integer(600)),
        ]))))
    };
    let descendant = world(&mut doc, "Identity", "ArialUnicodeMS");
    add(
        &mut doc,
        "U",
        dict(vec![
            ("Type", name("Font")),
            ("Subtype", name("Type0")),
            ("BaseFont", name("ArialUnicodeMS")),
            ("Encoding", name("Identity-H")),
            ("DescendantFonts", PdfObject::Array(vec![descendant])),
            ("ToUnicode", PdfObject::Reference(to_unicode)),
        ]),
    );
    let world_codes: Vec<u8> = (1..=chars.len() as u16)
        .flat_map(|c| c.to_be_bytes())
        .collect();

    // Unembedded CJK through predefined CMaps, as Acrobat-era documents do.
    for (key, encoding, ordering, base) in [
        ("J", "UniJIS-UCS2-H", "Japan1", "MS-Mincho"),
        ("G", "UniGB-UCS2-H", "GB1", "SimSun"),
        ("K", "UniKS-UCS2-H", "Korea1", "Batang"),
    ] {
        let descendant = {
            let d = world(&mut doc, ordering, base);
            if let PdfObject::Reference(id) = d {
                let mut font = doc.resolve(id).unwrap().as_dict().unwrap().clone();
                font.insert("Subtype".into(), name("CIDFontType0"));
                font.insert("DW".into(), PdfObject::Integer(1000));
                doc.set_object(id, PdfObject::Dictionary(font));
            }
            d
        };
        add(
            &mut doc,
            key,
            dict(vec![
                ("Type", name("Font")),
                ("Subtype", name("Type0")),
                ("BaseFont", name(base)),
                ("Encoding", name(encoding)),
                ("DescendantFonts", PdfObject::Array(vec![descendant])),
            ]),
        );
    }

    let content = format!(
        "BT /H 16 Tf 30 560 Td (Helvetica: The quick brown fox) Tj ET\n\
         BT /HB 16 Tf 30 535 Td (Helvetica-Bold: jumps over) Tj ET\n\
         BT /T 16 Tf 30 510 Td (Times-Roman: the lazy dog) Tj ET\n\
         BT /TI 16 Tf 30 485 Td (Times-Italic: \\(serif, slanted\\)) Tj ET\n\
         BT /C 16 Tf 30 460 Td (Courier: monospaced iiiWWW) Tj ET\n\
         BT /S 16 Tf 30 435 Td (abgdpS\\\"\\$ \\362) Tj ET\n\
         BT /Z 16 Tf 30 410 Td (4383n\\241) Tj ET\n\
         BT /U 18 Tf 30 375 Td <{world}> Tj ET\n\
         BT /J 20 Tf 30 335 Td <{japanese}> Tj ET\n\
         BT /G 20 Tf 30 300 Td <{chinese}> Tj ET\n\
         BT /K 20 Tf 30 265 Td <{korean}> Tj ET\n",
        world = hex(&world_codes),
        japanese = hex(&ucs2("日本語のテキスト、縦横")),
        chinese = hex(&ucs2("简体中文文本，汉字")),
        korean = hex(&ucs2("한국어 텍스트 한글")),
    );
    let content_id = doc.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        content.into_bytes(),
    )));
    let pages_id = ObjectId::new(900, 0);
    let page = doc.add_object(PdfObject::Dictionary(dict(vec![
        ("Type", name("Page")),
        ("Parent", PdfObject::Reference(pages_id)),
        (
            "MediaBox",
            PdfObject::Array(vec![
                PdfObject::Integer(0),
                PdfObject::Integer(0),
                PdfObject::Integer(600),
                PdfObject::Integer(600),
            ]),
        ),
        (
            "Resources",
            PdfObject::Dictionary(dict(vec![("Font", PdfObject::Dictionary(fonts))])),
        ),
        ("Contents", PdfObject::Reference(content_id)),
    ])));
    doc.set_object(
        pages_id,
        PdfObject::Dictionary(dict(vec![
            ("Type", name("Pages")),
            ("Kids", PdfObject::Array(vec![PdfObject::Reference(page)])),
            ("Count", PdfObject::Integer(1)),
        ])),
    );
    let catalog = doc.add_object(PdfObject::Dictionary(dict(vec![
        ("Type", name("Catalog")),
        ("Pages", PdfObject::Reference(pages_id)),
    ])));
    doc.set_trailer_key("Root", PdfObject::Reference(catalog));
    doc.save_as(&out).unwrap();
    eprintln!("wrote {out}");
}
