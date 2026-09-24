## Unreleased

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

These changes preserve the C ABI; the Dart API only gains the
`PdfRenderedPng` export. The checked-in Android and iOS binaries have been
regenerated from this source.

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
