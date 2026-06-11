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
