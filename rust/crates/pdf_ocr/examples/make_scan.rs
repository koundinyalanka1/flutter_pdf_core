//! Make a "scanned" PDF: lines typeset in Helvetica, rendered to grey pixels
//! at the given resolution, and saved as a PDF whose only content is that
//! image, as a scanner would deliver it.
//!
//!     cargo run --release -p pdf_ocr --example make_scan -- out.pdf 150 "line one" "line two"

use pdf_core::document::PdfDocument;
use pdf_core::filter::flate_encode;
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_ocr::GrayImage;
use pdf_render::{render_page, RenderOptions, RenderSize};

const SIZE: f64 = 14.0;
const LEADING: f64 = 20.0;
const MARGIN: f64 = 24.0;

fn page(doc: &mut PdfDocument, width: f64, height: f64, mut page: Dictionary) {
    let pages = doc.add_object(PdfObject::Null);
    page.insert("Type".into(), PdfObject::Name("Page".into()));
    page.insert("Parent".into(), PdfObject::Reference(pages));
    page.insert(
        "MediaBox".into(),
        PdfObject::Array(vec![
            0.into_pdf(),
            0.into_pdf(),
            PdfObject::Real(width),
            PdfObject::Real(height),
        ]),
    );
    let id = doc.add_object(PdfObject::Dictionary(page));
    doc.set_object(
        pages,
        PdfObject::Dictionary(Dictionary::from([
            ("Type".into(), PdfObject::Name("Pages".into())),
            (
                "Kids".into(),
                PdfObject::Array(vec![PdfObject::Reference(id)]),
            ),
            ("Count".into(), PdfObject::Integer(1)),
        ])),
    );
    let catalog = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Catalog".into())),
        ("Pages".into(), PdfObject::Reference(pages)),
    ])));
    doc.set_trailer_key("Root", PdfObject::Reference(catalog));
}

trait IntoPdf {
    fn into_pdf(self) -> PdfObject;
}

impl IntoPdf for i64 {
    fn into_pdf(self) -> PdfObject {
        PdfObject::Integer(self)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, dpi, lines) = (&args[0], args[1].parse::<f64>().expect("dpi"), &args[2..]);
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f64;
    let (width, height) = (
        2.0 * MARGIN + longest * SIZE * 0.56,
        2.0 * MARGIN + lines.len() as f64 * LEADING,
    );

    let mut typeset = PdfDocument::new_empty("1.7");
    let font = typeset.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Font".into())),
        ("Subtype".into(), PdfObject::Name("Type1".into())),
        ("BaseFont".into(), PdfObject::Name("Helvetica".into())),
        ("Encoding".into(), PdfObject::Name("WinAnsiEncoding".into())),
    ])));
    let mut content = format!(
        "BT /F1 {SIZE} Tf {LEADING} TL {MARGIN} {} Td\n",
        height - MARGIN - SIZE
    );
    for line in lines {
        let latin1: Vec<u8> = line
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        let escaped: String = latin1
            .iter()
            .map(|&b| match b {
                b'(' | b')' | b'\\' => format!("\\{}", b as char),
                32..=126 => (b as char).to_string(),
                _ => format!("\\{b:03o}"),
            })
            .collect();
        content += &format!("({escaped}) Tj T*\n");
    }
    content += "ET";
    let content = typeset.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        content.into_bytes(),
    )));
    page(
        &mut typeset,
        width,
        height,
        Dictionary::from([
            (
                "Resources".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "Font".into(),
                    PdfObject::Dictionary(Dictionary::from([(
                        "F1".into(),
                        PdfObject::Reference(font),
                    )])),
                )])),
            ),
            ("Contents".into(), PdfObject::Reference(content)),
        ]),
    );

    let render = render_page(
        &typeset,
        0,
        RenderOptions {
            size: RenderSize::Scale(dpi / 72.0),
            ..Default::default()
        },
    )
    .expect("render");
    let gray = GrayImage::from_rgba(
        render.width as usize,
        render.height as usize,
        &render.pixels,
    )
    .unwrap();
    let mut scan = PdfDocument::new_empty("1.7");
    let image = scan.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::from([
            ("Type".into(), PdfObject::Name("XObject".into())),
            ("Subtype".into(), PdfObject::Name("Image".into())),
            ("Width".into(), PdfObject::Integer(gray.width as i64)),
            ("Height".into(), PdfObject::Integer(gray.height as i64)),
            ("ColorSpace".into(), PdfObject::Name("DeviceGray".into())),
            ("BitsPerComponent".into(), PdfObject::Integer(8)),
            ("Filter".into(), PdfObject::Name("FlateDecode".into())),
        ]),
        flate_encode(&gray.pixels),
    )));
    let draw = format!("q {width:.3} 0 0 {height:.3} 0 0 cm /Im0 Do Q");
    let content = scan.add_object(PdfObject::Stream(PdfStream::new(
        Dictionary::new(),
        draw.into_bytes(),
    )));
    page(
        &mut scan,
        width,
        height,
        Dictionary::from([
            (
                "Resources".into(),
                PdfObject::Dictionary(Dictionary::from([(
                    "XObject".into(),
                    PdfObject::Dictionary(Dictionary::from([(
                        "Im0".into(),
                        PdfObject::Reference(image),
                    )])),
                )])),
            ),
            ("Contents".into(), PdfObject::Reference(content)),
        ]),
    );
    scan.save_as(out).expect("save");
    println!(
        "wrote {out}: {:.0} x {:.0} pt, {} x {} px",
        width, height, gray.width, gray.height
    );
}
