//! Pages crafted to make rendering run for minutes or take gigabytes. Each
//! test builds its document here, then renders it on a thread with the
//! 512 KiB stack Dart isolates get on 32-bit Android, under a deadline. A
//! render that runs out of work, time or memory stops, keeps what it drew,
//! and says the page is too complex.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use pdf_core::derived::DerivedCache;
use pdf_core::document::PdfDocument;
use pdf_core::filter::flate_encode;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::parser::Parser;
use pdf_core::stream::PdfStream;
use pdf_render::{
    render_page, render_page_cancellable, RenderCancel, RenderOptions, RenderSize, RenderedPage,
};

const STACK: usize = 512 * 1024;

/// What a render that ran out of work, time or memory says.
const TOO_COMPLEX: &str = "page too complex";

/// How long a test's render may take at most, on top of any time limit it
/// sets itself: five seconds in a release build, and a minute in a debug
/// build, which draws many times slower.
fn slack() -> Duration {
    Duration::from_secs(if cfg!(debug_assertions) { 60 } else { 5 })
}

/// Runs `render` on a thread with a small stack and returns its result and
/// how long it took. The test fails once `deadline` passes; the thread is
/// left running and ends with the test process.
fn render_within<T: Send + 'static>(
    name: &str,
    deadline: Duration,
    render: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(name.into())
        .stack_size(STACK)
        .spawn(move || {
            let start = Instant::now();
            let result = render();
            let _ = sender.send((result, start.elapsed()));
        })
        .unwrap();
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => panic!("{name} took longer than {deadline:?}"),
        Err(RecvTimeoutError::Disconnected) => panic!("{name} panicked"),
    }
}

/// Taken by every test here for as long as it runs. The tests of one file
/// share a process, and its memory is what they measure: one test's
/// pictures must not count against another's limit, nor its threads slow
/// another past its deadline.
fn alone() -> MutexGuard<'static, ()> {
    static ALONE: Mutex<()> = Mutex::new(());
    ALONE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A line of `/proc/self/status` in MiB: `VmHWM`, the most memory this
/// process has held, or `VmRSS`, what it holds now. None where Linux does
/// not report it.
fn status_mib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? / 1024)
}

/// The most memory this process has held, in MiB, where Linux reports it.
fn peak_memory_mib() -> Option<u64> {
    status_mib("VmHWM:")
}

/// Start measuring the peak afresh from what the process holds now, so the
/// memory a test spent building its document does not count. Returns false
/// where Linux does not allow it.
fn reset_peak_memory() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

fn integers<const N: usize>(values: [i64; N]) -> PdfObject {
    PdfObject::Array(values.map(PdfObject::Integer).to_vec())
}

/// An object written as PDF source.
fn object(source: &str) -> PdfObject {
    Parser::new(source.as_bytes()).parse_object().unwrap()
}

/// Replace the page's resources.
fn set_page_resources(doc: &mut PdfDocument, resources: PdfObject) {
    let page_id = doc.collect_page_ids().unwrap()[0];
    let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
    page.insert("Resources".into(), resources);
    doc.set_object(page_id, PdfObject::Dictionary(page));
}

fn stream(entries: Vec<(&str, PdfObject)>, data: Vec<u8>, compress: bool) -> PdfObject {
    let mut dict: Dictionary = entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    let data = if compress {
        dict.insert("Filter".into(), PdfObject::Name("FlateDecode".into()));
        flate_encode(&data)
    } else {
        data
    };
    dict.insert("Length".into(), PdfObject::Integer(data.len() as i64));
    PdfObject::Stream(PdfStream::new(dict, data))
}

/// A one-page document, 200 points square, whose compressed content stream
/// is `content`, with the XObjects `xobjects` (each a resource name and an
/// object) and any `extra` objects.
fn document(content: &[u8], xobjects: Vec<(String, PdfObject)>) -> PdfDocument {
    let mut doc = PdfDocument::new_empty("1.7");
    let mut names = Dictionary::new();
    for (name, object) in xobjects {
        let id = doc.add_object(object);
        names.insert(name, PdfObject::Reference(id));
    }
    let resources = Dictionary::from([("XObject".into(), PdfObject::Dictionary(names))]);
    let content = doc.add_object(stream(Vec::new(), content.to_vec(), true));
    let pages = ObjectId::new(9000, 0);
    let page = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Page".into())),
        ("Parent".into(), PdfObject::Reference(pages)),
        ("MediaBox".into(), integers([0, 0, 200, 200])),
        ("Resources".into(), PdfObject::Dictionary(resources)),
        ("Contents".into(), PdfObject::Reference(content)),
    ])));
    doc.set_object(
        pages,
        PdfObject::Dictionary(Dictionary::from([
            ("Type".into(), PdfObject::Name("Pages".into())),
            (
                "Kids".into(),
                PdfObject::Array(vec![PdfObject::Reference(page)]),
            ),
            ("Count".into(), PdfObject::Integer(1)),
        ])),
    );
    let catalog = doc.add_object(PdfObject::Dictionary(Dictionary::from([
        ("Type".into(), PdfObject::Name("Catalog".into())),
        ("Pages".into(), PdfObject::Reference(pages)),
    ])));
    doc.set_trailer_key("Root", PdfObject::Reference(catalog));
    doc
}

/// A page that draws the form `/X0`, which fills a square and draws itself
/// `copies` times: `copies` to the power of the nesting limit of twelve,
/// half a million forms for three copies, each with a clip the size of the
/// page. The surveyor's `mk_forms.py`.
fn form_bomb(copies: usize) -> PdfDocument {
    form_bomb_drawn_by(b"/X0 Do\n", copies)
}

/// [`form_bomb`], with the page's content `content`.
fn form_bomb_drawn_by(content: &[u8], copies: usize) -> PdfDocument {
    let mut doc = document(content, Vec::new());
    let form = ObjectId::new(8000, 0);
    let content = format!("{}0 0 1 1 re f\n", "/X0 Do ".repeat(copies));
    let resources = Dictionary::from([(
        "XObject".into(),
        PdfObject::Dictionary(Dictionary::from([(
            "X0".into(),
            PdfObject::Reference(form),
        )])),
    )]);
    doc.set_object(
        form,
        stream(
            vec![
                ("Type", PdfObject::Name("XObject".into())),
                ("Subtype", PdfObject::Name("Form".into())),
                ("BBox", integers([0, 0, 200, 200])),
                ("Resources", PdfObject::Dictionary(resources.clone())),
            ],
            content.into_bytes(),
            false,
        ),
    );
    // The page names the same form.
    set_page_resources(&mut doc, PdfObject::Dictionary(resources));
    doc
}

fn options(size: u32, time_limit: Option<Duration>) -> RenderOptions {
    RenderOptions {
        size: RenderSize::FitBox {
            width: size,
            height: size,
        },
        threads: 1,
        time_limit,
        ..Default::default()
    }
}

fn too_complex(page: &RenderedPage) -> bool {
    page.warnings.iter().any(|w| w.contains(TOO_COMPLEX))
}

/// Forms that draw themselves three and four times ran for more than a
/// minute. Under a time limit the render stops at it, with what it drew so
/// far and the warning.
#[test]
fn self_referencing_forms_stop_at_the_time_limit() {
    let _alone = alone();
    let limit = Duration::from_secs(1);
    for copies in [3, 4] {
        let doc = Arc::new(form_bomb(copies));
        let (page, took) = render_within("form bomb", limit + slack(), move || {
            render_page(&doc, 0, options(600, Some(limit))).unwrap()
        });
        eprintln!("{copies} copies at 600 px, 1 s limit: {took:?}");
        assert!(too_complex(&page), "{copies} copies: {:?}", page.warnings);
        // The square in the bottom left corner that the innermost forms
        // filled first is there.
        let corner = (599 * 600) * 4;
        assert_eq!(&page.pixels[corner..corner + 4], [0, 0, 0, 255]);
    }
}

/// A viewer may show only a page's first warning, and that the render
/// stopped part way matters most: it comes first, though another warning
/// sorts before it.
#[test]
fn a_stopped_render_says_so_first() {
    let _alone = alone();
    // A form whose stream cannot be decoded: a warning that sorts first.
    let mut doc = form_bomb_drawn_by(b"/Y0 Do /X0 Do\n", 3);
    let broken = doc.add_object(stream(
        vec![
            ("Subtype", PdfObject::Name("Form".into())),
            ("BBox", integers([0, 0, 200, 200])),
            ("Filter", PdfObject::Name("NoSuchFilter".into())),
        ],
        b"x".to_vec(),
        false,
    ));
    set_page_resources(
        &mut doc,
        object(&format!(
            "<< /XObject << /X0 8000 0 R /Y0 {} {} R >> >>",
            broken.number, broken.generation
        )),
    );
    let doc = Arc::new(doc);
    let limit = Duration::from_millis(300);
    let (page, _) = render_within("form bomb", limit + slack(), move || {
        render_page(&doc, 0, options(600, Some(limit))).unwrap()
    });
    assert!(page.warnings[0].contains(TOO_COMPLEX), "{:?}", page.warnings);
    assert!(page.warnings[1..].iter().any(|w| w.starts_with("form ")));
}

/// With no time limit, the cap on forms drawn ends the same pages.
#[test]
fn self_referencing_forms_stop_at_the_form_limit() {
    let _alone = alone();
    for copies in [3, 4] {
        let doc = Arc::new(form_bomb(copies));
        let (page, took) = render_within("form bomb", slack() * 2, move || {
            render_page(&doc, 0, options(10, None)).unwrap()
        });
        eprintln!("{copies} copies at 10 px, no time limit: {took:?}");
        assert!(too_complex(&page), "{copies} copies: {:?}", page.warnings);
    }
}

/// A render cancelled from another thread stops within moments.
#[test]
fn a_cancelled_render_stops() {
    let _alone = alone();
    let doc = Arc::new(form_bomb(4));
    let cancel = RenderCancel::new();
    let flag = cancel.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        flag.cancel();
    });
    let (page, took) = render_within("cancelled render", slack(), move || {
        render_page_cancellable(&doc, 0, options(600, None), &cancel).unwrap()
    });
    canceller.join().unwrap();
    eprintln!("cancelled after 200 ms, returned after {took:?}");
    assert!(page.warnings.iter().any(|w| w.contains("cancelled")));
    assert!(took < Duration::from_millis(200) + slack());
}

/// Renders `content` and fails when the peak memory of the process grew by
/// `limit_mib` or more while it was drawn.
fn render_in_bounded_memory(name: &str, content: Vec<u8>, limit_mib: u64) -> RenderedPage {
    let doc = document(&content, Vec::new());
    drop(content);
    let measured = reset_peak_memory();
    let before = peak_memory_mib();
    let (page, took) = render_within(name, Duration::from_secs(20) + slack(), move || {
        render_page(&doc, 0, options(200, None)).unwrap()
    });
    if let (true, Some(before), Some(after)) = (measured, before, peak_memory_mib()) {
        let grown = after.saturating_sub(before);
        eprintln!("{name}: rendered in {took:?}, peak grew by {grown} MiB");
        assert!(grown < limit_mib, "{name} grew the peak by {grown} MiB");
    }
    page
}

/// Sixty million numbers before one operator took 31.6 s and 3.3 GB to
/// render, and sixty million operators most of a gigabyte to parse. One
/// test, so that the cases' peaks are measured one at a time.
#[test]
fn pages_of_millions_of_operators_render_in_bounded_memory() {
    let _alone = alone();
    let mut numbers = b"0 ".repeat(60_000_000);
    numbers.extend_from_slice(b"m\n0 0 10 10 re f\n");
    let page = render_in_bounded_memory("60M operands", numbers, 500);
    assert_eq!(&page.pixels[..4], [255, 255, 255, 255]);
    assert_eq!(
        &page.pixels[(199 * 200) * 4..(199 * 200) * 4 + 4],
        [0, 0, 0, 255]
    );

    // Sixty million operators: more than may be parsed, so part of the page
    // is left out, and the render says so.
    let mut operators = b"0 0 10 10 re f\n".to_vec();
    operators.extend_from_slice(&b"n ".repeat(60_000_000));
    let page = render_in_bounded_memory("60M operators", operators, 500);
    assert!(too_complex(&page), "{:?}", page.warnings);
    assert_eq!(
        &page.pixels[(199 * 200) * 4..(199 * 200) * 4 + 4],
        [0, 0, 0, 255]
    );
}

/// A form of half a million operators drawn two hundred times is more work
/// than a render may do: it stops part way, with the warning.
#[test]
fn work_past_the_budget_stops_the_render() {
    let _alone = alone();
    let form = stream(
        vec![
            ("Subtype", PdfObject::Name("Form".into())),
            ("BBox", integers([0, 0, 200, 200])),
        ],
        b"1 w ".repeat(500_000),
        true,
    );
    let doc = Arc::new(document(
        "/F Do ".repeat(200).as_bytes(),
        vec![("F".into(), form)],
    ));
    let (page, took) = render_within("work budget", slack() * 2, move || {
        render_page(&doc, 0, options(100, None)).unwrap()
    });
    eprintln!("100M operators: stopped after {took:?}");
    assert!(too_complex(&page), "{:?}", page.warnings);
}

/// Distinct pictures that decode to 16 MB each, more of them than a page
/// may decode: the ones past the budget are skipped, with a warning, and
/// the rest drawn. Half a gigabyte of decoding takes a while in a debug
/// build.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "decodes half a gigabyte; run with --release"
)]
fn pictures_past_the_page_budget_are_skipped() {
    let _alone = alone();
    let side = 4096;
    let samples =
        pdf_render::encode_jpeg(&vec![0u8; side * side], side as u32, side as u32, 1, 50).unwrap();
    let pictures = 40;
    let xobjects = (0..pictures)
        .map(|index| {
            let mut dict = Dictionary::from([
                ("Subtype".into(), PdfObject::Name("Image".into())),
                ("Width".into(), PdfObject::Integer(side as i64)),
                ("Height".into(), PdfObject::Integer(side as i64)),
                ("ColorSpace".into(), PdfObject::Name("DeviceGray".into())),
                ("BitsPerComponent".into(), PdfObject::Integer(8)),
                ("Filter".into(), PdfObject::Name("DCTDecode".into())),
            ]);
            dict.insert("Length".into(), PdfObject::Integer(samples.len() as i64));
            (
                format!("I{index}"),
                PdfObject::Stream(PdfStream::new(dict, samples.clone())),
            )
        })
        .collect();
    // Each picture fills a five-point column of the page.
    let content: String = (0..pictures)
        .map(|index| format!("q 5 0 0 200 {} 0 cm /I{index} Do Q\n", index * 5))
        .collect();
    let doc = Arc::new(document(content.as_bytes(), xobjects));
    let (page, took) = render_within("picture budget", Duration::from_secs(20), move || {
        render_page(&doc, 0, options(200, None)).unwrap()
    });
    eprintln!(
        "{pictures} pictures of 16 MB: {took:?}, {:?}",
        page.warnings
    );
    assert!(
        page.warnings
            .iter()
            .any(|w| w.contains("exceed the renderer's memory limit")),
        "{:?}",
        page.warnings
    );
    // The first pictures are drawn, the last is not.
    assert_eq!(&page.pixels[..4], [0, 0, 0, 255]);
    assert_eq!(&page.pixels[199 * 4..200 * 4], [255, 255, 255, 255]);
}

/// What this process holds now, in MiB, where Linux reports it.
fn resident_memory_mib() -> Option<u64> {
    status_mib("VmRSS:")
}

/// Twelve forms, each drawing the next before eleven million operators of
/// its own: each parses to about the most any content may take, 132 MB, and
/// each was held while the ones inside it were drawn, 1.6 GB in all. What
/// one band holds for nested content is bounded now: the forms past it are
/// not parsed in full, and the render says the page is too complex.
#[test]
fn nested_forms_hold_bounded_parsed_content() {
    let _alone = alone();
    let mut content = b"/N Do\n".to_vec();
    content.extend_from_slice(&b"n\n".repeat(11_000_000));
    let compressed = flate_encode(&content);
    drop(content);
    let mut doc = document(b"/N Do\n", Vec::new());
    let mut next: Option<ObjectId> = None;
    for _ in 0..12 {
        let mut dict = object("<< /Subtype /Form /BBox [0 0 200 200] /Filter /FlateDecode >>")
            .as_dict()
            .unwrap()
            .clone();
        if let Some(inner) = next {
            dict.insert(
                "Resources".into(),
                object(&format!(
                    "<< /XObject << /N {} {} R >> >>",
                    inner.number, inner.generation
                )),
            );
        }
        dict.insert("Length".into(), PdfObject::Integer(compressed.len() as i64));
        next = Some(doc.add_object(PdfObject::Stream(PdfStream::new(
            dict,
            compressed.clone(),
        ))));
    }
    let outer = next.unwrap();
    set_page_resources(
        &mut doc,
        object(&format!(
            "<< /XObject << /N {} {} R >> >>",
            outer.number, outer.generation
        )),
    );
    let measured = reset_peak_memory();
    let before = peak_memory_mib();
    let (page, took) = render_within("nested forms", Duration::from_secs(20) + slack(), move || {
        render_page(&doc, 0, options(200, None)).unwrap()
    });
    if let (true, Some(before), Some(after)) = (measured, before, peak_memory_mib()) {
        let grown = after.saturating_sub(before);
        eprintln!("12 nested forms of 11M operators: {took:?}, peak grew by {grown} MiB");
        assert!(grown < 500, "nested forms grew the peak by {grown} MiB");
    }
    assert!(too_complex(&page), "{:?}", page.warnings);
}

/// A Type3 font of a hundred glyphs whose procedures each parse to more than
/// three megabytes, in a document the app keeps open: the font kept every
/// procedure it parsed for as long as the document stayed open, 360 MB for
/// these. It keeps a few megabytes now, and parses the rest each time they
/// are drawn.
#[test]
fn type3_fonts_keep_bounded_parsed_procedures() {
    let _alone = alone();
    let glyphs = 100;
    let mut procedure = b"1000 0 0 0 1000 1000 d1 0 0 1000 1000 re f\n".to_vec();
    procedure.extend_from_slice(&b"n\n".repeat(300_000));
    let compressed = flate_encode(&procedure);
    drop(procedure);
    let codes: String = (1..=glyphs).map(|code| format!("{code:02X}")).collect();
    let mut doc = document(
        format!("BT /T3 2 Tf 0 100 Td <{codes}> Tj ET").as_bytes(),
        Vec::new(),
    );
    let mut char_procs = Dictionary::new();
    let mut names = String::new();
    for code in 1..=glyphs {
        let id = doc.add_object(stream(
            vec![("Filter", PdfObject::Name("FlateDecode".into()))],
            compressed.clone(),
            false,
        ));
        char_procs.insert(format!("g{code}"), PdfObject::Reference(id));
        names.push_str(&format!(" /g{code}"));
    }
    let mut font = object(&format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
         /FontMatrix [0.001 0 0 0.001 0 0] \
         /Encoding << /Type /Encoding /Differences [1{names}] >> \
         /FirstChar 1 /LastChar {glyphs} /Widths [{}] >>",
        "1000 ".repeat(glyphs)
    ))
    .as_dict()
    .unwrap()
    .clone();
    font.insert("CharProcs".into(), PdfObject::Dictionary(char_procs));
    let font = doc.add_object(PdfObject::Dictionary(font));
    set_page_resources(
        &mut doc,
        object(&format!(
            "<< /Font << /T3 {} {} R >> >>",
            font.number, font.generation
        )),
    );
    // As the app keeps a document open while it is read.
    doc.derived = DerivedCache::enabled();
    let doc = Arc::new(doc);

    let before = resident_memory_mib();
    let kept = Arc::clone(&doc);
    let (first, took) = render_within("Type3 procedures", Duration::from_secs(20) + slack(), move || {
        render_page(&kept, 0, options(200, None)).unwrap()
    });
    let after = resident_memory_mib();
    if let (Some(before), Some(after)) = (before, after) {
        let kept = after.saturating_sub(before);
        eprintln!("100 Type3 procedures of 3.6 MB: {took:?}, {kept} MiB still held");
        assert!(kept < 150, "the font kept {kept} MiB of procedures");
    }
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    // Every glyph is drawn: a black square two points wide, the page is 200
    // points, and so is the render.
    let row = 99 * 200 * 4;
    for x in [1, 50, 101, 150, 197] {
        let at = row + x * 4;
        assert_eq!(&first.pixels[at..at + 4], [0, 0, 0, 255], "glyph at {x}");
    }
    // Drawn again from what the font kept and what it parses afresh, the
    // page is the same.
    let (second, _) = render_within("Type3 procedures again", Duration::from_secs(20) + slack(), move || {
        render_page(&doc, 0, options(200, None)).unwrap()
    });
    assert!(first.pixels == second.pixels);
}

/// A Type3 glyph whose procedure shows itself eight times: eight to the
/// power of the nesting limit, which never finished. The time limit ends
/// it, with what it drew and the warning.
#[test]
fn self_drawing_type3_glyphs_stop_at_the_time_limit() {
    let _alone = alone();
    let mut doc = document(b"BT /T3 200 Tf 0 0 Td (A) Tj ET", Vec::new());
    let font = ObjectId::new(7000, 0);
    let glyph = doc.add_object(stream(
        Vec::new(),
        b"1000 0 0 0 1000 1000 d1 0 0 125 125 re f BT /T3 125 Tf 0 0 Td (AAAAAAAA) Tj ET".to_vec(),
        true,
    ));
    doc.set_object(
        font,
        object(&format!(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
             /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /g {} 0 R >> \
             /Encoding << /Type /Encoding /Differences [65 /g] >> \
             /FirstChar 65 /LastChar 65 /Widths [1000] \
             /Resources << /Font << /T3 7000 0 R >> >> >>",
            glyph.number
        )),
    );
    set_page_resources(&mut doc, object("<< /Font << /T3 7000 0 R >> >>"));
    let limit = Duration::from_secs(1);
    let doc = Arc::new(doc);
    let (page, took) = render_within("Type3 bomb", limit + slack(), move || {
        render_page(&doc, 0, options(200, Some(limit))).unwrap()
    });
    eprintln!("Type3 glyph drawing itself 8 times, 1 s limit: {took:?}");
    assert!(too_complex(&page), "{:?}", page.warnings);
    // The square the outermost glyph filled at the bottom left is there.
    let corner = (199 * 200) * 4;
    assert_eq!(&page.pixels[corner..corner + 4], [0, 0, 0, 255]);
}

/// A tiling pattern of 144 tiles, each of two million operators: 288
/// million in all, which ran for many seconds. The work budget and the time
/// limit end it.
#[test]
fn heavy_tiling_patterns_stop_at_the_time_limit() {
    let _alone = alone();
    let mut tile = b"0 0 5 5 re f\n".to_vec();
    tile.extend_from_slice(&b"1 w\n".repeat(2_000_000));
    let pattern = stream(
        vec![
            ("Type", PdfObject::Name("Pattern".into())),
            ("PatternType", PdfObject::Integer(1)),
            ("PaintType", PdfObject::Integer(1)),
            ("TilingType", PdfObject::Integer(1)),
            ("BBox", integers([0, 0, 10, 10])),
            ("XStep", PdfObject::Integer(20)),
            ("YStep", PdfObject::Integer(20)),
        ],
        tile,
        true,
    );
    let mut doc = document(b"/Pattern cs /P scn 0 0 200 200 re f\n", Vec::new());
    let pattern = doc.add_object(pattern);
    set_page_resources(
        &mut doc,
        object(&format!(
            "<< /Pattern << /P {} {} R >> >>",
            pattern.number, pattern.generation
        )),
    );
    for limit in [Some(Duration::from_secs(1)), None] {
        let doc = Arc::new(doc.clone());
        let (page, took) = render_within(
            "tiling pattern",
            Duration::from_secs(1) + slack() * 2,
            move || render_page(&doc, 0, options(200, limit)).unwrap(),
        );
        eprintln!("144 tiles of 2M operators, limit {limit:?}: {took:?}");
        assert!(too_complex(&page), "{:?}", page.warnings);
        // The first tile, at the bottom left, is drawn.
        let corner = (199 * 200) * 4;
        assert_eq!(&page.pixels[corner..corner + 4], [0, 0, 0, 255]);
    }
}

/// Thirty-two pictures of 16 MB fill a page's budget for decoded pictures,
/// so the last one, whose soft mask hides it, is drawn without its mask, and
/// the page says so. That picture was then kept without its mask for the
/// document's next render: the next page, which draws it alone and has room
/// for its mask, drew it in black and said nothing.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "decodes half a gigabyte; run with --release"
)]
fn a_picture_whose_mask_was_refused_is_not_kept_without_it() {
    let _alone = alone();
    let side = 4096;
    let samples =
        pdf_render::encode_jpeg(&vec![0u8; side * side], side as u32, side as u32, 1, 50).unwrap();
    let picture = |filter: &str, data: Vec<u8>, extra: Vec<(&str, PdfObject)>| {
        let mut entries = vec![
            ("Subtype", PdfObject::Name("Image".into())),
            ("Width", PdfObject::Integer(side as i64)),
            ("Height", PdfObject::Integer(side as i64)),
            ("ColorSpace", PdfObject::Name("DeviceGray".into())),
            ("BitsPerComponent", PdfObject::Integer(8)),
            ("Filter", PdfObject::Name(filter.into())),
        ];
        entries.extend(extra);
        stream(entries, data, false)
    };
    // The fillers in five-point columns, then a black picture, under a soft
    // mask that hides all of it, over the right of the page.
    let fillers = 31;
    let mut content: String = (0..fillers)
        .map(|index| format!("q 5 0 0 200 {} 0 cm /I{index} Do Q\n", index * 5))
        .collect();
    let masked = "q 20 0 0 200 180 0 cm /X Do Q\n";
    content.push_str(masked);
    let mut doc = document(content.as_bytes(), Vec::new());
    let mut names = Dictionary::new();
    for index in 0..fillers {
        let filler = doc.add_object(picture("DCTDecode", samples.clone(), Vec::new()));
        names.insert(format!("I{index}"), PdfObject::Reference(filler));
    }
    let zeros = flate_encode(&vec![0u8; side * side]);
    let mask = doc.add_object(picture("FlateDecode", zeros.clone(), Vec::new()));
    let black = doc.add_object(picture(
        "FlateDecode",
        zeros,
        vec![("SMask", PdfObject::Reference(mask))],
    ));
    names.insert("X".into(), PdfObject::Reference(black));
    let resources = PdfObject::Dictionary(Dictionary::from([(
        "XObject".into(),
        PdfObject::Dictionary(names),
    )]));
    set_page_resources(&mut doc, resources.clone());
    // A second page that draws the masked picture alone.
    let first_page = doc.collect_page_ids().unwrap()[0];
    let mut second_page = doc.resolve(first_page).unwrap().as_dict().unwrap().clone();
    let contents = doc.add_object(stream(Vec::new(), masked.as_bytes().to_vec(), true));
    second_page.insert("Contents".into(), PdfObject::Reference(contents));
    let second_page = doc.add_object(PdfObject::Dictionary(second_page));
    let pages = ObjectId::new(9000, 0);
    let mut tree = doc.resolve(pages).unwrap().as_dict().unwrap().clone();
    tree.insert(
        "Kids".into(),
        PdfObject::Array(vec![
            PdfObject::Reference(first_page),
            PdfObject::Reference(second_page),
        ]),
    );
    tree.insert("Count".into(), PdfObject::Integer(2));
    doc.set_object(pages, PdfObject::Dictionary(tree));
    // As the app keeps a document open while it is read.
    doc.derived = DerivedCache::enabled();
    let doc = Arc::new(doc);

    let kept = Arc::clone(&doc);
    let (first, _) = render_within("refused mask", Duration::from_secs(30), move || {
        render_page(&kept, 0, options(200, None)).unwrap()
    });
    assert!(
        first
            .warnings
            .iter()
            .any(|w| w.contains("exceed the renderer's memory limit")),
        "{:?}",
        first.warnings
    );
    assert_eq!(&first.pixels[..4], [0, 0, 0, 255]);
    let (second, _) = render_within("the masked picture alone", Duration::from_secs(30), move || {
        render_page(&doc, 1, options(200, None)).unwrap()
    });
    let at = (100 * 200 + 190) * 4;
    eprintln!(
        "the masked picture alone: {:?}, {:?}",
        second.warnings,
        &second.pixels[at..at + 4]
    );
    assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    assert_eq!(&second.pixels[at..at + 4], [255, 255, 255, 255]);
}

/// Three pictures that decode to 70 MB each, 210 MB between them, drawn one
/// over another three times. The renderer keeps only the newest picture that
/// large, so each draw decodes its picture again; every decode was taken from
/// the page's budget for decoded pictures as if it were another picture, so
/// the eighth ran the budget out and the last two draws were skipped. A
/// picture decoded again holds no more memory than the copy let go: it is
/// taken from the budget once a render, and the page is drawn in full.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "decodes 600 MB of pictures; run with --release"
)]
fn pictures_decoded_again_are_paid_for_once() {
    let _alone = alone();
    let (width, height) = (4800, 4900);
    let colours = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]];
    let xobjects = colours
        .iter()
        .enumerate()
        .map(|(index, colour)| {
            let samples = flate_encode(&colour.repeat(width * height));
            let picture = stream(
                vec![
                    ("Subtype", PdfObject::Name("Image".into())),
                    ("Width", PdfObject::Integer(width as i64)),
                    ("Height", PdfObject::Integer(height as i64)),
                    ("ColorSpace", PdfObject::Name("DeviceRGB".into())),
                    ("BitsPerComponent", PdfObject::Integer(8)),
                    ("Filter", PdfObject::Name("FlateDecode".into())),
                ],
                samples,
                false,
            );
            (format!("I{index}"), picture)
        })
        .collect();
    let content = "q 200 0 0 200 0 0 cm /I0 Do /I1 Do /I2 Do Q\n".repeat(3);
    let doc = Arc::new(document(content.as_bytes(), xobjects));
    let measured = reset_peak_memory();
    let before = peak_memory_mib();
    let (page, took) = render_within(
        "pictures decoded again",
        Duration::from_secs(30),
        move || render_page(&doc, 0, options(200, None)).unwrap(),
    );
    eprintln!(
        "3 pictures of 70 MB drawn 3 times: {took:?}, {:?}",
        page.warnings
    );
    assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    // The last picture drawn is the blue one.
    assert_eq!(&page.pixels[..4], [0, 0, 255, 255]);
    if let (true, Some(before), Some(after)) = (measured, before, peak_memory_mib()) {
        let grown = after.saturating_sub(before);
        eprintln!("pictures decoded again: peak grew by {grown} MiB");
        assert!(
            grown < 512,
            "pictures decoded again grew the peak by {grown} MiB"
        );
    }
}
