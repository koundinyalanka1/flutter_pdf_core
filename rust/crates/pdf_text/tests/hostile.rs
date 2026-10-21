//! Pages crafted to make reading their text abort, or cost time or memory
//! out of all proportion to their size. Each test builds its document here,
//! then reads it on a thread with the 512 KiB stack Dart isolates get on
//! 32-bit Android, under a deadline.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use pdf_core::document::PdfDocument;
use pdf_core::filter::flate_encode;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_core::stream::PdfStream;
use pdf_text::extractor::extract_page_text;
use pdf_text::layout::extract_page_layout;

const STACK: usize = 512 * 1024;

/// How long reading a crafted page may take: ten seconds in a release
/// build, and two minutes in a debug build, which parses many times slower.
fn deadline() -> Duration {
    Duration::from_secs(if cfg!(debug_assertions) { 120 } else { 10 })
}

/// Runs `read` on a thread with a small stack and returns its result and
/// how long it took. The test fails once the deadline passes; the thread is
/// left running and ends with the test process.
fn read_within_deadline<T: Send + 'static>(
    name: &str,
    read: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(name.into())
        .stack_size(STACK)
        .spawn(move || {
            let start = Instant::now();
            let result = read();
            let _ = sender.send((result, start.elapsed()));
        })
        .unwrap();
    match receiver.recv_timeout(deadline()) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => panic!("{name} took longer than {:?}", deadline()),
        Err(RecvTimeoutError::Disconnected) => panic!("{name} panicked"),
    }
}

/// Taken by every test here for as long as it runs: the tests of one file
/// share a process, and its memory is what they measure.
fn alone() -> std::sync::MutexGuard<'static, ()> {
    static ALONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ALONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The most memory this process has held, in MiB (`VmHWM`), where Linux
/// reports it.
fn peak_memory_mib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? / 1024)
}

/// Start measuring the peak afresh from what the process holds now, so the
/// memory a test spent building its document does not count. Returns false
/// where Linux does not allow it.
fn reset_peak_memory() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

/// A one-page document whose content stream is `content`, compressed, with
/// the font `/F1` and any `extra` objects. A form XObject `/X0` is object
/// 9, when `extra` holds one.
fn document(content: &[u8], extra: Vec<(u32, PdfObject)>) -> PdfDocument {
    let mut doc = PdfDocument::new_empty("1.7");
    let font = Dictionary::from([
        ("Type".into(), PdfObject::Name("Font".into())),
        ("Subtype".into(), PdfObject::Name("Type1".into())),
        ("BaseFont".into(), PdfObject::Name("Helvetica".into())),
    ]);
    doc.set_object(ObjectId::new(5, 0), PdfObject::Dictionary(font));
    let resources = Dictionary::from([
        (
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "F1".into(),
                PdfObject::Reference(ObjectId::new(5, 0)),
            )])),
        ),
        (
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "X0".into(),
                PdfObject::Reference(ObjectId::new(9, 0)),
            )])),
        ),
    ]);
    let compressed = flate_encode(content);
    let stream = Dictionary::from([
        ("Filter".into(), PdfObject::Name("FlateDecode".into())),
        ("Length".into(), PdfObject::Integer(compressed.len() as i64)),
    ]);
    doc.set_object(
        ObjectId::new(4, 0),
        PdfObject::Stream(PdfStream::new(stream, compressed)),
    );
    let page = Dictionary::from([
        ("Type".into(), PdfObject::Name("Page".into())),
        ("Parent".into(), PdfObject::Reference(ObjectId::new(2, 0))),
        (
            "MediaBox".into(),
            PdfObject::Array([0, 0, 200, 200].map(PdfObject::Integer).to_vec()),
        ),
        ("Resources".into(), PdfObject::Dictionary(resources)),
        ("Contents".into(), PdfObject::Reference(ObjectId::new(4, 0))),
    ]);
    doc.set_object(ObjectId::new(3, 0), PdfObject::Dictionary(page));
    let pages = Dictionary::from([
        ("Type".into(), PdfObject::Name("Pages".into())),
        (
            "Kids".into(),
            PdfObject::Array(vec![PdfObject::Reference(ObjectId::new(3, 0))]),
        ),
        ("Count".into(), PdfObject::Integer(1)),
    ]);
    doc.set_object(ObjectId::new(2, 0), PdfObject::Dictionary(pages));
    let catalog = Dictionary::from([
        ("Type".into(), PdfObject::Name("Catalog".into())),
        ("Pages".into(), PdfObject::Reference(ObjectId::new(2, 0))),
    ]);
    doc.set_object(ObjectId::new(1, 0), PdfObject::Dictionary(catalog));
    doc.set_trailer_key("Root", PdfObject::Reference(ObjectId::new(1, 0)));
    for (number, object) in extra {
        doc.set_object(ObjectId::new(number, 0), object);
    }
    doc
}

/// A form XObject, object 9, that shows `(x)` and then draws itself `copies`
/// times: `copies` to the power of the nesting limit, unless the number of
/// forms followed is capped.
fn self_referencing_form(copies: usize) -> (u32, PdfObject) {
    let content = format!(
        "BT /F1 12 Tf 10 10 Td (x) Tj ET {}",
        "/X0 Do ".repeat(copies)
    );
    let resources = Dictionary::from([
        (
            "Font".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "F1".into(),
                PdfObject::Reference(ObjectId::new(5, 0)),
            )])),
        ),
        (
            "XObject".into(),
            PdfObject::Dictionary(Dictionary::from([(
                "X0".into(),
                PdfObject::Reference(ObjectId::new(9, 0)),
            )])),
        ),
    ]);
    let dict = Dictionary::from([
        ("Type".into(), PdfObject::Name("XObject".into())),
        ("Subtype".into(), PdfObject::Name("Form".into())),
        (
            "BBox".into(),
            PdfObject::Array([0, 0, 200, 200].map(PdfObject::Integer).to_vec()),
        ),
        ("Resources".into(), PdfObject::Dictionary(resources)),
        ("Length".into(), PdfObject::Integer(content.len() as i64)),
    ]);
    (
        9,
        PdfObject::Stream(PdfStream::new(dict, content.into_bytes())),
    )
}

/// Read the text of `content` within the deadline, and fail when the peak
/// memory of the process grew by `limit_mib` or more while it was read.
/// Returns the text.
fn read_in_bounded_memory(name: &str, content: Vec<u8>, limit_mib: u64) -> String {
    let doc = document(&content, Vec::new());
    drop(content);
    let measured = reset_peak_memory();
    let before = peak_memory_mib();
    let (text, took) = read_within_deadline(name, move || extract_page_text(&doc, 0));
    if let (true, Some(before), Some(after)) = (measured, before, peak_memory_mib()) {
        let grown = after.saturating_sub(before);
        eprintln!("{name}: read in {took:?}, peak grew by {grown} MiB");
        assert!(grown < limit_mib, "{name} grew the peak by {grown} MiB");
    }
    text.unwrap()
}

/// 60 million numbers and then one operator, which a 117 KB file holds.
/// Every number was kept for the operator: reading the page peaked at
/// 5.6 GB. One test, so that the cases' peaks are measured one at a time,
/// and the other tests here wait for it.
#[test]
fn pages_of_millions_of_operands_are_read_in_bounded_memory() {
    let _alone = alone();
    let mut numbers = b"0 ".repeat(60_000_000);
    numbers.extend_from_slice(b"m\nBT /F1 12 Tf 10 10 Td (after) Tj ET\n");
    let text = read_in_bounded_memory("60M operands", numbers, 500);
    assert_eq!(text, "after");

    // The same numbers inside one array, then text after it.
    let mut array = b"[".to_vec();
    array.extend_from_slice(&b"0 ".repeat(60_000_000));
    array.extend_from_slice(b"] TJ BT /F1 12 Tf 10 10 Td (after) Tj ET\n");
    let text = read_in_bounded_memory("60M array items", array, 500);
    assert_eq!(text, "after");

    // Sixty million operators, each held while the page was read, after the
    // text: more than a page may have, so the reading stops at the limit,
    // with the text read before it.
    let mut operators = b"BT /F1 12 Tf 10 10 Td (before) Tj ET\n".to_vec();
    operators.extend_from_slice(&b"n ".repeat(60_000_000));
    let text = read_in_bounded_memory("60M operators", operators, 500);
    assert_eq!(text, "before");

    // Forty million saved graphics states, never restored: fewer than the
    // operators a page may have, so the text after them is still read.
    let mut saves = b"q ".repeat(40_000_000);
    saves.extend_from_slice(b"BT /F1 12 Tf 10 10 Td (after) Tj ET\n");
    let text = read_in_bounded_memory("40M saves", saves, 500);
    assert_eq!(text, "after");

    // One string of 100 million characters.
    let mut string = b"BT /F1 12 Tf 10 10 Td (".to_vec();
    string.extend_from_slice(&b"a".repeat(100_000_000));
    string.extend_from_slice(b") Tj ET\n");
    let text = read_in_bounded_memory("a 100M-character string", string, 500);
    assert!(text.len() >= 1_000_000 && text.bytes().all(|b| b == b'a'));
}

/// A form that draws itself eight times: 8^9 forms through the nesting
/// limit of eight, which took minutes to read. The cap on forms followed
/// keeps it to a fraction of a second, and the text is still read.
#[test]
fn a_self_referencing_form_is_read_within_the_deadline() {
    let _alone = alone();
    for copies in [3, 4, 8] {
        let doc = document(
            b"BT /F1 12 Tf 10 100 Td (page) Tj ET /X0 Do",
            vec![self_referencing_form(copies)],
        );
        let doc = std::sync::Arc::new(doc);
        let shared = std::sync::Arc::clone(&doc);
        let (text, took) = read_within_deadline("form bomb", move || extract_page_text(&shared, 0));
        let text = text.unwrap();
        assert!(text.starts_with("page"), "{copies} copies: {text:.40}");
        assert!(text.contains('x'));
        eprintln!("{copies} copies: text read in {took:?}");
        let (layout, _) =
            read_within_deadline("form bomb layout", move || extract_page_layout(&doc, 0));
        assert!(layout.unwrap().text.starts_with("page"));
    }
}
