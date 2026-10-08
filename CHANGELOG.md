## Unreleased

* OCR, written from scratch (new `pdf_ocr` crate). Recognizes printed
  Latin-script text on scanned pages. Pages scanned skewed, sideways or upside
  down are handled. `pdf_ocr_page_json` (`PdfCore.ocrPage`) reads a page and
  returns its lines, word boxes and selectable layout.
  `pdf_make_searchable_json` (`PdfCore.makeSearchable`) saves a copy with an
  invisible text layer over every scanned page, and `pdf_apply_ocr_json`
  (`PdfCore.applyOcr`) writes results recognized page by page or by another
  engine. Pages that already carry text are left alone unless forced. The
  layer renders invisibly, its word boxes match the scan, and text
  extraction, selection, search and AI export read it like any other text.
  The recognizer's model (about 1 MB) is compiled in; `tools/ocr_train`
  reproduces it.
* Plain-text extraction (`pdf_extract_text`, AI export) now places spaces and
  line breaks along each run's baseline, as text layout already did. Rotated
  text no longer breaks into one line per run. Words set with a unit font
  size and a scaled text matrix no longer gain spaces at kerning gaps.
* Text extraction tracks the text rendering mode; `page_text_stats` counts a
  page's visible, invisible and unmapped text.
* `MacRomanEncoding` decodes the whole high range. Before, the fi and fl
  ligatures that macOS writes for "file" or "flat" came out as "Þ" and "ß".
* Pin parsed documents: `pdf_document_open`/`pdf_document_close`
  (`PdfCore.openDocument`/`closeDocument`) keep one parse in memory, and
  read-only calls on the same path and password share it while the file is
  unchanged, instead of each reading and parsing the whole file. Viewers that
  make many calls at once no longer multiply memory use by their concurrency.
* Read common writer quirks as intact files instead of recovering them as
  damaged: index entries in use at offset 0 are free, `endobj` may be omitted
  before the next object, the index or the end of the file, integers too large
  for 64 bits saturate, and references to impossible object numbers read as
  null (they still never wrap onto a real object).
* Encode passwords as each revision requires: PDFDocEncoding for RC4/AES-128
  (revisions 2–4), UTF-8 after SASLprep for AES-256, with the raw UTF-8 bytes
  as a fallback. AES-256 encryption applies SASLprep; files encrypted by
  earlier versions still open. Adds the `stringprep` dependency.
* Enforce render pixel budgets for extreme page sizes and aspect ratios;
  honour tiny fit boxes and reject invalid scales or zero pixel budgets.
* Resolve indirect page rotations consistently in rendering and size queries.
* Treat null page attributes as absent when inheriting settings, including
  when rebuilding a page tree after page operations.
* Reject non-finite crop/media boxes and normalize large rotation deltas
  before addition to avoid integer overflow.
* Reject out-of-range PDF object numbers and generations instead of truncating
  them to references to different objects.
* Return `PAGE_OUT_OF_RANGE` for negative page indices at the C/Flutter
  boundary. Failed renders clear output lengths, dimensions, and prior warnings.
* Keep AI chunks within their Unicode character limit, including overlap,
  and retain the source-page range of overlapping text. Rust chunk limits
  below 64 are now honoured; zero is treated as one character.
* Reject objects nested more than 128 levels deep instead of overflowing the
  stack, which aborts the process even behind the FFI panic guard.
* Write output files atomically, including `pdf_encrypt`: a failed save
  leaves an existing destination untouched.
* Link Android libraries for 16 KB memory pages (Android 15+), and build
  them with `--locked`.
* Export `PdfRenderedPng` from the package library.

These changes extend the C ABI with the two document-pinning functions and
the three OCR functions. The Dart API gains `openDocument`, `closeDocument`,
the `PdfRenderedPng` export, and `ocrPage`, `makeSearchable(Async)` and
`applyOcr(Async)` with their `PdfOcr…` types. Bindings treat the new functions
as optional, so older native libraries keep working; OCR calls then throw
`OCR_UNAVAILABLE`. The checked-in Android, iOS and macOS binaries were
regenerated for the document-pinning changes. They do not yet include OCR:
rebuild them with `scripts/build_*.sh`.

## 0.1.0

Milestones 3–12 of the from-scratch Rust PDF core:

* Page tree model with inheritance + clean rebuilds (M3)
* Split / delete / reorder / duplicate pages with compact renumbering (M4)
* Merge any number of documents (M5)
* Rotate, media/crop boxes, full /Info metadata editing incl. UTF-16 (M6)
* FlateDecode (+PNG/TIFF predictors), ASCIIHex/85, xref streams, /Prev
  chains, hybrid files, object streams (M7)
* Text extraction: text-state machine, WinAnsi/MacRoman/Standard +
  /Differences, ToUnicode CMaps, Identity-H CID fonts, form XObjects (M8)
* AI-ready JSON/NDJSON export with paragraph-aware overlapping chunks (M9)
* Encryption: opens RC4 40/128, AES-128, AES-256 (R5/R6) files; writes
  AES-256 (R6, PDF 2.0); decrypt-on-save (M10)
* Flutter integration via hand-written C ABI + dart:ffi, sync + isolate
  async APIs, typed PdfException codes (M11)
* Android (cargo-ndk), iOS (XCFramework), macOS (universal dylib) build
  scripts and GitHub Actions CI (M12)

## 0.0.1

* Initial plugin scaffold; Rust parser/inspector (M1) and writer with
  round-trip tests (M2).
