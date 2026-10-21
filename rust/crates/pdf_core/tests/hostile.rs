//! Files crafted to make opening a PDF abort, or cost time or memory out of
//! all proportion to their size. Each test builds its file here, then opens
//! it on a thread with the 512 KiB stack Dart isolates get on 32-bit
//! Android, under a deadline.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::filter::flate_encode;
use pdf_core::object::{ObjectId, PdfObject};

const STACK: usize = 512 * 1024;

/// How long opening a crafted file may take: one second in a release
/// build, and ten in a debug build, which parses many times slower.
fn deadline() -> Duration {
    Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 1 })
}

/// Runs `open` on a thread with a small stack and returns its result and
/// how long it took. The test fails once the deadline passes; the thread is
/// left running and ends with the test process.
fn open_within_deadline<T: Send + 'static>(
    name: &str,
    open: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(name.into())
        .stack_size(STACK)
        .spawn(move || {
            let start = Instant::now();
            let result = open();
            let _ = sender.send((result, start.elapsed()));
        })
        .unwrap();
    match receiver.recv_timeout(deadline()) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => panic!("{name} took longer than {:?}", deadline()),
        Err(RecvTimeoutError::Disconnected) => panic!("{name} panicked"),
    }
}

/// The most memory this process has held, in KiB (`VmHWM`), where Linux
/// reports it.
fn peak_memory_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Fails when the process's peak memory grew by `limit_mib` or more since
/// `before`. Other tests run at the same time, so the limit leaves room for
/// them; each of them stays far below it.
fn assert_peak_grew_less_than(before: Option<u64>, limit_mib: u64, name: &str) {
    if let (Some(before), Some(after)) = (before, peak_memory_kib()) {
        let grown = after.saturating_sub(before) / 1024;
        assert!(grown < limit_mib, "{name} grew the peak by {grown} MiB");
    }
}

/// Opens `pdf` and returns its page count and the objects named by
/// `numbers` that are the literal string `(x)`.
fn open_and_count(pdf: Vec<u8>, numbers: std::ops::Range<u32>) -> Result<(Option<u32>, usize)> {
    let document = PdfDocument::from_bytes(&pdf)?;
    let expected = PdfObject::LiteralString(b"x".to_vec());
    let found = numbers
        .filter(|&number| document.resolve(ObjectId::new(number, 0)) == Some(&expected))
        .count();
    Ok((document.page_count(), found))
}

/// A PDF 1.5 file with its catalog (1), page tree (2) and page (4) as plain
/// objects, and object stream 3 holding `header` then `body` under the
/// count `/N`. Its cross-reference stream lists the first `listed` objects
/// from number 10 on as stored in stream 3.
fn object_stream_pdf(header: &[u8], body: &[u8], count: u64, listed: u32) -> Vec<u8> {
    let mut out = b"%PDF-1.5\n".to_vec();
    let mut offsets = [0u32; 5];
    for (number, object) in [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[4 0 R]/Count 1>>"),
        (4, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>"),
    ] {
        offsets[number] = out.len() as u32;
        out.extend_from_slice(format!("{number} 0 obj\n{object}\nendobj\n").as_bytes());
    }
    let compressed = flate_encode(&[header, body].concat());
    offsets[3] = out.len() as u32;
    out.extend_from_slice(
        format!(
            "3 0 obj\n<</Type/ObjStm/N {count}/First {}/Filter/FlateDecode/Length {}>>\nstream\n",
            header.len(),
            compressed.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&compressed);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    // Rows of /W [1 4 2]: the kind, then the offset or the stream's number,
    // then the generation or the index in the stream.
    let mut rows = vec![0u8; 7];
    for offset in &offsets[1..] {
        rows.push(1);
        rows.extend_from_slice(&offset.to_be_bytes());
        rows.extend_from_slice(&[0, 0]);
    }
    for index in 0..listed {
        rows.push(2);
        rows.extend_from_slice(&3u32.to_be_bytes());
        rows.extend_from_slice(&(index as u16).to_be_bytes());
    }
    let compressed = flate_encode(&rows);
    let xref = out.len();
    out.extend_from_slice(
        format!(
            "7 0 obj\n<</Type/XRef/Size {}/W[1 4 2]/Index[0 5 10 {listed}]/Root 1 0 R/Filter/FlateDecode/Length {}>>\nstream\n",
            10 + listed,
            compressed.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&compressed);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    out.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// The catalog, page tree and page every recovery test starts with, as
/// objects 1 to 3 after a PDF 1.4 header, and where each of them starts.
fn pdf_with_one_page() -> (Vec<u8>, [usize; 3]) {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = [0; 3];
    for (offset, object) in offsets.iter_mut().zip([
        "1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n",
        "2 0 obj <</Type/Pages/Kids[3 0 R]/Count 1>> endobj\n",
        "3 0 obj <</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]>> endobj\n",
    ]) {
        *offset = out.len();
        out.extend_from_slice(object.as_bytes());
    }
    (out, offsets)
}

#[test]
fn an_object_stream_claiming_billions_of_objects_is_an_error_not_an_abort() {
    // Reserving room for the claimed count asked for 32 GB, and then 16 TB,
    // which aborts the process.
    for count in [2_000_000_000, 1_000_000_000_000] {
        let pdf = object_stream_pdf(b"10 0 ", b"null", count, 1);
        let before = peak_memory_kib();
        let (result, _) = open_within_deadline("huge /N", move || open_and_count(pdf, 10..11));
        // The FFI reports a structure error as ERROR.
        assert!(
            matches!(&result, Err(PdfError::Structure(message)) if message.contains("object stream")),
            "{result:?}"
        );
        assert_peak_grew_less_than(before, 128, "huge /N");
    }
}

#[test]
fn thirty_two_thousand_objects_in_one_object_stream_open_in_under_a_second() {
    // Reading the stream's header again for every object made this 145 KB
    // file take more than two minutes.
    let count = 32_000;
    let mut header = Vec::new();
    let mut body = Vec::new();
    for index in 0..count {
        header.extend_from_slice(format!("{} {} ", 10 + index, body.len()).as_bytes());
        body.extend_from_slice(b"(x) ");
    }
    let pdf = object_stream_pdf(&header, &body, count.into(), count);
    let (result, took) = open_within_deadline("32,000 compressed objects", move || {
        open_and_count(pdf, 10..10 + count)
    });
    assert_eq!(result.unwrap(), (Some(1), 32_000), "{took:?}");
}

#[test]
fn compressed_objects_made_to_overlap_are_an_error_not_a_copy_each() {
    // A thousand numbers that all point at one array of ten thousand
    // numbers had that array parsed and kept once for each: ten million
    // values, 460 MB.
    let count = 1000;
    let header: Vec<u8> = (0..count)
        .flat_map(|index| format!("{} 0 ", 10 + index).into_bytes())
        .collect();
    let body = format!("[{}]", "0 ".repeat(10_000));
    let pdf = object_stream_pdf(&header, body.as_bytes(), count.into(), count);
    let before = peak_memory_kib();
    let (result, _) = open_within_deadline("overlapping compressed objects", move || {
        open_and_count(pdf, 10..10 + count)
    });
    assert!(
        matches!(&result, Err(PdfError::Structure(message)) if message.contains("overlap")),
        "{result:?}"
    );
    assert_peak_grew_less_than(before, 256, "overlapping compressed objects");
}

#[test]
fn a_thousand_xref_sections_sharing_one_large_table_recover_in_under_a_second() {
    // Every small section names the same 50,000-entry table as its /Prev,
    // and the file's startxref is damaged, so recovery reads them all. It
    // read the 1 MB table once for every section, for 17 seconds.
    let (entries, sections) = (50_000, 1000);
    let (mut pdf, offsets) = pdf_with_one_page();
    let objects = offsets.map(|offset| format!("{offset:010} 00000 n \n"));
    let table = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {entries}\n0000000000 65535 f \n").as_bytes());
    for number in 1..entries {
        pdf.extend_from_slice(objects[number % 3].as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n<</Size {entries}/Root 1 0 R>>\n").as_bytes());
    for _ in 0..sections {
        pdf.extend_from_slice(
            format!("xref\n0 0\ntrailer\n<</Size 4/Root 1 0 R/Prev {table}>>\n").as_bytes(),
        );
    }
    pdf.extend_from_slice(b"startxref\nnot-a-number\n%%EOF\n");
    assert!(pdf.len() > 1_000_000);

    let (result, took) = open_within_deadline("1000 sections", move || {
        let document = PdfDocument::from_bytes(&pdf)?;
        Ok::<_, PdfError>((
            document.page_count(),
            document.xref.entries.len(),
            document.recovery_warnings.len(),
        ))
    });
    // The recovered index still holds the shared table's entries.
    assert_eq!(result.unwrap(), (Some(1), entries, 1), "{took:?}");
}

#[test]
fn xref_sections_made_to_overlap_spend_a_budget_bound_to_the_file_size() {
    // Two thousand sections each name as /Prev a different stream that never
    // ends, so reading each one searches the 2 MB that follow for
    // `endstream`; that took 4 seconds.
    let sections = 2000;
    let (mut pdf, _) = pdf_with_one_page();
    let fragment = "9 0 obj <<>> stream\n";
    let section =
        |prev: usize| format!("xref\n0 0\ntrailer\n<</Size 4/Root 1 0 R/Prev {prev:010}>>\n");
    let first_fragment = pdf.len() + sections * section(0).len();
    for index in 0..sections {
        pdf.extend_from_slice(section(first_fragment + index * fragment.len()).as_bytes());
    }
    assert_eq!(pdf.len(), first_fragment);
    pdf.extend_from_slice(fragment.repeat(sections).as_bytes());
    pdf.push(b'%');
    pdf.extend(std::iter::repeat_n(b'x', 2_000_000));
    pdf.extend_from_slice(b"\nstartxref\nnot-a-number\n%%EOF\n");

    let (result, took) = open_within_deadline("overlapping sections", move || {
        PdfDocument::from_bytes(&pdf).map(|document| document.page_count())
    });
    assert_eq!(result.unwrap(), Some(1), "{took:?}");
}

#[test]
fn stream_lengths_read_through_a_damaged_index_spend_a_budget_bound_to_the_file_size() {
    // Sixteen thousand streams take their /Length from objects the index
    // places inside one long run of nested strings, so reading each length
    // also read the strings that follow it, to their end. The index itself
    // is intact apart from one wrong offset, which sends the file to
    // recovery with that index; the 2 MB file took 7 seconds.
    let streams: u32 = 16_000;
    let (mut pdf, [catalog, pages, _]) = pdf_with_one_page();
    let mut offsets = vec![0; 10 + 2 * streams as usize];
    offsets[1..4].copy_from_slice(&[catalog, pages, 999_999_999]);
    for index in 0..streams {
        let number = 10 + index;
        offsets[number as usize] = pdf.len();
        pdf.extend_from_slice(
            format!(
                "{number} 0 obj <</Length {} 0 R>>\nstream\nabc\nendstream\nendobj\n",
                number + streams
            )
            .as_bytes(),
        );
    }
    for index in 0..streams {
        let number = 10 + streams + index;
        offsets[number as usize] = pdf.len();
        pdf.extend_from_slice(format!("{number} 0 obj 3 endobj (").as_bytes());
    }
    pdf.extend_from_slice(")".repeat(streams as usize).as_bytes());
    pdf.push(b'\n');
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
    for offset in &offsets {
        let entry = if *offset == 0 {
            "0000000000 65535 f \n".to_owned()
        } else {
            format!("{offset:010} 00000 n \n")
        };
        pdf.extend_from_slice(entry.as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );

    let (result, took) = open_within_deadline("indirect stream lengths", move || {
        let document = PdfDocument::from_bytes(&pdf)?;
        Ok::<_, PdfError>((document.page_count(), document.recovery_warnings.len()))
    });
    assert_eq!(result.unwrap(), (Some(1), 1), "{took:?}");
}
