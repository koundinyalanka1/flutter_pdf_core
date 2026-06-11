# Milestone log

A from-scratch PDF processing library in Rust, integrated with Flutter via a hand-written C ABI and `dart:ffi`. No third-party PDF dependency anywhere — only small utility crates (flate2 for zlib, RustCrypto primitives for AES/MD5/SHA).

Run all native tests with `cd rust && cargo test --workspace`.

## M1 — Parser + inspector ✅

Lexer (`pdf_core::lexer`), object model (`object`), recursive-descent parser (`parser`), classic xref tables (`xref`), document loader + inspector (`document`). Tolerant of junk before `%PDF-`, comments, and odd whitespace.

**Next steps from here:** lexer string/name edge cases are covered, but object streams holding lexer state across nested parses would benefit from fuzzing (see Roadmap).

## M2 — Writer + round-trip ✅

`writer.rs` serializes the full object graph with a fresh classic xref table, correct stream `/Length` regeneration, name/string escaping, and a binary marker line. Round-trip tests confirm parse → write → parse stability.

**Improvements made during verification (this pass):** trailer now also drops `/Encrypt` and xref-stream bookkeeping keys; stream reads now honor a direct `/Length` and only fall back to scanning for `endstream`, so binary payloads containing that keyword survive.

## M3 — Proper page tree model ✅

`pdf_ops::page_tree`: depth-first walk of arbitrarily nested `/Pages` trees with cycle/depth protection, inherited-attribute resolution (`Resources`, `MediaBox`, `CropBox`, `Rotate`), and `rebuild_page_tree` which materializes inheritance onto each page and rebuilds a flat, clean tree. `PdfDocument::page_count()` now counts real pages instead of trusting `/Count`.

**Next steps:** balanced (fan-out 32+) tree output for 1000+-page documents; `/Parent`-free orphan-page recovery.

## M4 — Split / delete / reorder ✅

`pdf_ops::split`: `extract_pages` copies the transitive closure of each kept page into a fresh compactly-numbered document (annotations, contents, resources follow automatically; inherited attributes are materialized first). `delete_pages` rebuilds in place and garbage-collects unreachable objects. `reorder_pages` validates a full permutation. `split_into_pages` emits one document per page.

**Known limitation:** annotations that link to *other* pages drag those page objects in as orphans (harmless, slightly larger output).

## M5 — Merge ✅

`pdf_ops::merge`: any number of documents, object renumbering + reference rewriting via the same closure-copy engine, one flat output tree, `/Info` carried from the first source. Highest input header version wins.

**Next steps:** outline (bookmark) merging; deduplicating identical font/resource objects across sources.

## M6 — Rotate / crop / metadata ✅

`pdf_ops::rotate`: relative + absolute rotation (validated multiples of 90, inheritance-aware), media/crop box get/set with degenerate-rect rejection. `pdf_ops::metadata`: full `/Info` read/write with PDFDoc-vs-UTF-16BE text-string handling; `None` = untouched, `""` = delete.

**Next steps:** XMP (`/Metadata` stream) sync so viewers that prefer XMP see the same values.

## M7 — Stream parsing + FlateDecode ✅

`pdf_core::filter`: FlateDecode (flate2/zlib), PNG predictors 10–15 (Sub/Up/Average/Paeth), TIFF predictor 2, ASCIIHex, ASCII85, filter chains with per-filter `DecodeParms`. Xref parsing now also handles **xref streams** (PDF 1.5+, `/W`/`/Index`), `/Prev` chains for incrementally-updated files, and hybrid `/XRefStm` files. Document loading expands **object streams** (`/Type /ObjStm`).

**Next steps:** LZWDecode and RunLengthDecode (rare in modern files); writing compressed object streams for smaller output.

## M8 — Content stream text extraction ✅

`pdf_text`: content-stream operator parser (arrays, dicts, inline-image skipping), faithful text state machine (Tm/Td/TD/T*/TL/Tc/Tw/Tz/Ts, TJ kerning), font decoding (WinAnsi/MacRoman/Standard + `/Differences`, ToUnicode CMaps, Identity-H CID fonts, `/Widths` + CID `/W` advances), form XObject recursion, and position-based word/line/paragraph heuristics.

**Known limitations:** `bfrange` array form is skipped; glyph-name table is an AGL subset; no right-to-left reordering or vertical writing modes.

## M9 — AI-ready JSON/NDJSON export ✅

`pdf_ai`: paragraph-aware chunker with configurable size/overlap and per-chunk page ranges; `export::to_json` (single self-describing document, schema `flutter_pdf_core/export/v1`) and `export::to_ndjson` (header line + one chunk per line) — ready to stream into embedding/RAG pipelines for a local model.

**Next steps:** token-based (rather than character-based) sizing; optional per-chunk position metadata for citation highlighting.

## M10 — Password encryption/decryption ✅

`pdf_core::crypt`: Standard security handler.
* **Decrypt:** RC4 40/128-bit (R2–R4), AES-128 (R4/AESV2), AES-256 (R5 and R6/AESV3) — user *or* owner password, including the R6 iterated 2.B hash.
* **Encrypt:** AES-256 R6 (PDF 2.0) with proper `/O`, `/U`, `/OE`, `/UE`, `/Perms` and fresh `/ID`. The normal writer always emits decrypted output, so "remove password" is just open-with-password + save.

**Known limitations:** passwords are used as UTF-8 without SASLprep normalization (matches most non-ASCII-password tooling in practice); public-key (certificate) security handlers unsupported.

## M11 — Flutter integration (dart:ffi) ✅

Chosen approach: **hand-written C ABI + `dart:ffi`** (zero codegen, zero runtime deps — lighter than flutter_rust_bridge, per project goals).

* `pdf_ffi`: 16 `extern "C"` functions (inspect/page-count/metadata/split/delete/reorder/merge/rotate/crop/text/AI-export/encrypt/decrypt), panic-safe (`catch_unwind`), thread-local `pdf_last_error()` with stable machine-readable codes, 1-based page-range strings (`"1-3,5"`).
* Dart: `lib/src/bindings.dart` (raw FFI) + `lib/src/pdf_core_api.dart` (`PdfCore` static API, `PdfMetadata`/`PdfInfo` models, `PdfException` with `isEncrypted`/`isWrongPassword`, and `...Async` variants on background isolates).
* Library loading per platform, overridable with `PDF_CORE_LIB_PATH` for tests.

## M12 — Packaging + CI ✅

* `scripts/build_android.sh` — cargo-ndk → `android/src/main/jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/libpdf_ffi.so` (bundled automatically by the Gradle library project).
* `scripts/build_ios.sh` — static XCFramework (device + fat simulator) vendored by the iOS podspec with `-force_load` so dart:ffi symbols survive linking.
* `scripts/build_macos.sh` — universal dylib vendored by the macOS podspec.
* `pubspec.yaml` upgraded to a dual (method-channel + `ffiPlugin`) plugin.
* `.github/workflows/ci.yml` — cargo fmt/clippy/test, an Android cross-build, and flutter analyze/test on every push.

**Next steps:** prebuilt-binary releases (GitHub Releases) so plugin consumers don't need a Rust toolchain; Windows/Linux desktop packaging.

---

## Roadmap (beyond M12)

1. **Robustness:** xref-recovery scan for corrupt files (rebuild by scanning `N G obj`); fuzzing harness (`cargo fuzz`) for lexer/parser/filters.
2. **Size/perf:** write objects into compressed object streams; `memmap2` for lazy loading of multi-hundred-MB files; benchmark suite.
3. **Features:** page insertion/blank-page creation, watermark/stamp content, image extraction, AcroForm field reading, incremental save (preserves signatures), LZW/RLE filters, XMP metadata.
4. **AI:** layout-aware extraction (columns, tables), token-count chunking, embeddings-friendly section titles.
