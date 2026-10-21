# flutter_pdf_core: notes for coding agents

This is the guide for every coding agent (Codex, Cursor, Claude and others)
working in this repository, on its own or as part of the DocHelper app.
`CLAUDE.md` imports it, so Claude Code reads the same rules.

## What this is

- **The PDF engine of DocHelper.** DocHelper is a Flutter app, Android
  first, that the owner is growing from a PDF toolkit into an all-in-one
  office app (word processor, spreadsheets, presentations and PDFs) so his
  father can do office work on a phone. Every engine is written from
  scratch in Rust.
- **Repository:** `koundinyalanka1/flutter_pdf_core`, branch `main`. It is
  a git submodule of the app (`koundinyalanka1/pdfhelper`) at
  `packages/flutter_pdf_core`, and the app's `pubspec.yaml` depends on it
  by path.
- **Kind of file:** PDF only. Parse and inspect, split, merge, delete,
  reorder, rotate, crop, metadata, text and text layout, rendering, OCR,
  JPEG-to-PDF, passwords, form filling, annotations, signatures (visible
  stamps, not cryptographic), page marks, compression and pages as
  pictures.
- **Native library:** `libpdf_ffi` (`libpdf_ffi.so` on Android,
  `PdfFfi.xcframework` on iOS, `libpdf_ffi.dylib` on macOS).
- **Dart API:** `PdfCore` (`package:flutter_pdf_core/flutter_pdf_core.dart`),
  with `PdfException` and the edit types (`PdfEdits`, `PdfFieldValue`,
  `PdfAnnotationEdit`, `PdfPageMark`, ...).
- **It does not use `packages/doc_common`.** Unlike the text, sheet, word,
  slide and archive engines, this workspace has no relative path into the
  app, so it builds from a plain clone of this repository. The PDF export
  of Word, Excel and PowerPoint documents is `doc_common`'s `doc_pdf`, not
  this engine.
- **Who calls it in the app:** `lib/services/pdf_core_service.dart`,
  `pdf_service.dart`, `pdf_raster.dart`, `pdf_text_search.dart`,
  `pdf_text_selection_service.dart`; the screens `pdf_edit_screen.dart`,
  `pdf_stamp_screen.dart`, `compress_screen.dart`,
  `page_images_screen.dart`, `image_viewer_screen.dart` (make a PDF) and
  `tools_screen.dart`; and the deferred AI code in `lib/ai/`.
- **The app's own guide** is `AGENTS.md` at the app's root. Where work
  stands is in the app's `PLAN.md` and `docs/PHASE10_PLAN.md`.

## The owner's rules that apply here

- **Everything is Rust, written from scratch.** No C or C++ is written or
  vendored. `extern "C"` on exported Rust functions is only the calling
  convention `dart:ffi` needs. Do not add PDF, document, ZIP, XML, font or
  Office crates.
- **Allowed dependencies** are the small primitives already in use:
  - `flate2` (deflate);
  - RustCrypto `aes`, `cbc`, `sha2`, `md-5`, and `getrandom`;
  - `serde`/`serde_json`, only at the FFI boundary (the derives on result
    and option types exist so they can cross it);
  - `thiserror`.

  The PDF engine also uses standalone image codecs: JPEG 2000 and JBIG2
  are vendored in `rust/vendor/` (`hayro-jpeg2000` 0.4.1, `hayro-jbig2`
  0.3.1, which pulls in `hayro-ccitt` 0.4.0), and JPEG is decoded with
  `jpeg-decoder` 0.3. Also already in the tree: `stringprep` (SASLprep for
  AES-256 passwords), `tempfile` (atomic saves) and `anyhow` (`pdf_cli`;
  listed in `pdf_ffi`'s manifest). Add nothing new. Data (CMaps, AFM
  metrics, open-licensed fonts, the OCR model) is fine.
- **Never damage a user's file.**
  - Edit in place. For a PDF that means an incremental update: the
    original bytes stay as they are and the changes are appended
    (`pdf_edit`). Keep objects you do not understand. (In the Office
    engines the same rule reads: copy unchanged ZIP entries raw and keep
    XML you do not understand.)
  - Save atomically: `PdfWriter::write_bytes_atomic` or
    `PdfDocument::save_as` (a sibling temporary file, `fsync`, then a
    rename). The app-wide helper `doc_ffi::write_file_atomically` is not
    available here.
  - Formats that cannot be written back are saved as their modern
    equivalent, and the app says so. A PDF is always written as a PDF; the
    nearest case here is XFA, which filling a form drops (see below).
- **Never change package identifiers.** Here: the Dart package name
  `flutter_pdf_core`, the Android plugin package
  `com.yourmateapps.flutter_pdf_core` and `FlutterPdfCorePlugin`. In the
  app: `com.yourmateapps.pdfhelper` (Android `applicationId`, namespace,
  Kotlin package, iOS bundle ID, the `.debug` suffix scheme) and the Dart
  package name `pdfhelper`. Keep the library names `libpdf_ffi` and
  `PdfFfi.xcframework` too: the app's scripts, podspecs and
  `scripts/check_android_native.py` look for them.
- **Git.**
  - Commit or push only when the owner asks.
  - Work on `main`. No feature branches (an old `production-audit-fixes`
    branch on the remote is not for new work).
  - Commit messages are one plain imperative sentence that says what the
    change does for the user, without a type prefix. From `git log`:
    "Fill forms, annotate, sign and stamp PDFs, keeping the original bytes
    and signatures intact"; "Render pages faster on several threads and
    stay pixel-identical".
  - Commit and push in this repository first, then commit the updated
    gitlink in the app.
- **AI features are deferred, not abandoned.** `pdf_ai` and
  `PdfCore.exportForAi` feed the app's `lib/ai/`, which `Features.ai =
  false` hides. Keep them compiling and tested; do not delete them.
- On the owner's Mac, device checks go on the emulator
  `pdfhelper_api36_16kb`, not his phones.

## Layout

The Rust workspace is `rust/` (edition 2021, `Cargo.lock` committed).

| Crate | What it holds |
| --- | --- |
| `pdf_core` | `lexer`, `parser`, `object`, `xref` (tables, streams, `/Prev` chains), `filter` (Flate, LZW, ASCII85 and others with predictors), `stream`, `document` (`PdfDocument`, object streams), `recovery` (rebuilding a damaged index), `crypt` (RC4, AES-128, AES-256), `writer` (`PdfWriter`, atomic saves), `derived` (the cache pinned documents share), `error` (`PdfError`) |
| `pdf_ops` | `page_tree`, `split` (closure copy, also used by merge), `merge`, `rotate` (rotate, crop), `metadata`, `compose` (JPEG to PDF), `preserve` (AcroForm fields and bookmarks kept through split and merge) |
| `pdf_text` | `content_stream`, `font`, `cmap`, `agl`, `standard14`, `text_state`, `extractor` (plain text), `layout` (glyph geometry for selection); `data/` (`cmaps.bin`, AFM metrics, glyph lists); example `build_cmap_bundle` regenerates `cmaps.bin` |
| `pdf_render` | The CPU rasterizer: `canvas`, `geom`, `page*.rs` (operators, paints, shadings and meshes, transparency, forms, annotations, optional content, strokes, text, limits, bands for threads), `font/` (TrueType, CFF, Type 1, fallback, system fonts), `image`, `image_codecs`, `ccitt`, `png`, `jpeg_encode` (our baseline JPEG encoder); `fonts/` (Roboto), `data/fallback-fonts.bin` (Noto subsets); examples `render_png`, `contact_sheet`, `build_fallback_bundle`, `make_fallback_fixture`; test `tests/font_widths.rs` |
| `pdf_edit` | Forms (`forms`), annotations (`annots`), page marks (`marks`), appearance streams (`content`, `font`, `image`, `frame`), the incremental writer (`editor`) and the JSON model (`model`) |
| `pdf_compress` | Smaller copies (pictures scaled and re-encoded, streams packed, object streams and a cross-reference stream) and pages as PNG or JPEG |
| `pdf_ocr` | OCR from scratch: `binarize`, `components`, `layout`, `normalize`, `net` (CNN plus bidirectional LSTM), `ctc`, `engine`, `layer` and `glyphless` (invisible text layers), `pdf`; `models/latin.ocrm` (about 1 MB, compiled in by the default `embedded-model` feature); examples `make_scan`, `ocr_debug`, `ocr_timing`; tests `golden.rs`, `orientation.rs`, `pdf.rs` |
| `pdf_ai` | `chunker` and `export`: JSON/NDJSON chunks for local models (deferred AI feature) |
| `pdf_ffi` | `src/api.rs`: every exported function; `build.rs`: 16 KB pages on Android; builds `staticlib`, `cdylib` and `rlib` |
| `pdf_cli` | Developer CLI over the whole library (`src/main.rs`; run it without arguments for the usage) |

Everything else:

| Path | What |
| --- | --- |
| `rust/vendor/` | Vendored `hayro-jpeg2000` and `hayro-jbig2` with our resource limits; `vendor/README.md` lists the changes to keep when upgrading |
| `rust/fixtures/` | Small PDFs for the Rust tests (encrypted samples, a scan, simple pages) |
| `lib/flutter_pdf_core.dart` | Library entry; exports the public types |
| `lib/src/bindings.dart` | Raw `dart:ffi` signatures, `PdfBindings`, library loading, optional lookups for newer functions |
| `lib/src/pdf_core_api.dart` | `PdfCore`, `PdfException`, result types (`PdfInfo`, `PdfRenderedPage`, `PdfPageTextLayout`, `PdfOcrPage`, ...) |
| `lib/src/pdf_edit.dart` | Form, annotation, mark and compression types (`PdfForm`, `PdfEdits`, `PdfCompressReport`, ...) |
| `lib/flutter_pdf_core_method_channel.dart`, `..._platform_interface.dart` | Plugin template leftovers (`getPlatformVersion` only) |
| `test/` | Dart tests (7 files, `sample.pdf`); the native ones need `PDF_CORE_LIB_PATH` |
| `example/` | Example app; `example/integration_test` exercises the FFI on a device |
| `android/` | Gradle library, template Kotlin plugin, `src/main/jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/libpdf_ffi.so` (**committed**) |
| `ios/` | Podspec (force-loads the static library), `Package.swift`, `Frameworks/PdfFfi.xcframework` (**committed**) |
| `macos/` | Podspec (vendors the dylib), `Package.swift`, `Frameworks/libpdf_ffi.dylib`, universal (**committed**) |
| `scripts/` | `build_android.sh`, `build_ios.sh`, `build_macos.sh` |
| `tools/ocr_train/` | Offline training of the OCR model (Python and PyTorch; never ships) |
| `tools/pdf_viewer/` | A standalone desktop utility with its own Cargo workspace (not shipped) |
| `docs/` | `ARCHITECTURE.md`, `MILESTONES.md`, `OCR_PROGRESS.md`, `last_session.txt` (older notes; some are stale, see Pitfalls) |
| `.github/workflows/ci.yml` | fmt and clippy (both advisory), `cargo test --workspace`, an arm64 Android build, `flutter analyze --no-fatal-infos`, `flutter test` |
| `CHANGELOG.md` | Add to its "Unreleased" section when behaviour changes |

The binaries are committed in this repository, unlike the per-format
engines' binaries, which are git-ignored. `rust/target/` is ignored.

## Building and testing

Every command below was run on the Linux cloud machine on 2026-10-15 (the
counts again on 2026-10-18, after phase 10's batch B3) and succeeded, with
cargo 1.97 and Flutter at `/opt/flutter/bin`:

```bash
export PATH=/opt/flutter/bin:$HOME/.cargo/bin:$PATH

# The Rust workspace (from rust/).
cd rust
cargo test --workspace --release        # 572 passed, 0 failed (about 70 s with the release build cached)
cargo clippy --workspace --release --all-targets   # completes, with warnings (see below)
cargo build -p pdf_ffi --release        # target/release/libpdf_ffi.so for Dart tests on Linux

# The CLI and examples on a real file.
cargo run --release -p pdf_cli -- inspect form.pdf
cargo run --release -p pdf_cli -- fields form.pdf
cargo run --release -p pdf_cli -- text form.pdf 1
cargo run --release -p pdf_cli -- render form.pdf 1 page1.png 1.5   # prints render warnings
cargo run --release -p pdf_cli -- edit form.pdf edits.json filled.pdf
cargo run --release -p pdf_cli -- roundtrip filled.pdf
cargo run --release -p pdf_cli -- compress form.pdf small.pdf 150 70
cargo run --release -p pdf_cli -- images form.pdf page- jpeg 100      # page-1.jpg, page-2.jpg, ...
cargo run --release -p pdf_render --example contact_sheet -- in.pdf sheet.png

# The Dart package (from the package root).
cd ..
PDF_CORE_LIB_PATH=$PWD/rust/target/release/libpdf_ffi.so flutter test   # 25 passed
flutter analyze --no-fatal-infos        # 34 infos, no warnings or errors
```

The real file used was `test_corpus/government/irs_fw9.pdf` from the app
(the corpus is git-ignored; the app's `scripts/download_*_corpus.py`
rebuild it). A minimal `edits.json`:

```json
{"fields": [{"name": "topmostSubform[0].Page1[0].f1_01[0]", "value": "Ada Lovelace"}],
 "marks": [{"text": "Page {page} of {pages}", "position": "bottomCenter"}]}
```

The filled file kept the original 140,815 bytes as an exact prefix
(`cmp -n`), as an incremental update should.

**Test counts** (`cargo test --workspace --release`): `pdf_core` 79 and
6 (`hostile`), `pdf_ops` 35 and 1 doc test, `pdf_text` 81 and 2
(`hostile`), `pdf_render` 270, 1 (`font_widths`) and 13 (`hostile`),
`pdf_edit` 19, `pdf_compress` 7, `pdf_ocr` 17 and 8 (`golden` 2,
`orientation` 1, `pdf` 5), `pdf_ai` 9, `pdf_ffi` 17, `pdf_cli` 0. That is
565 for the engine; the vendored codecs add 7 (`hayro-jbig2` 5,
`hayro-jpeg2000` 1 and 1 doc test), because path dependencies inside
`rust/` are workspace members. The `hostile` suites build crafted files in
the tests, run them on 512 KiB-stack threads under deadlines and check
memory from `VmHWM`; one render test that decodes 600 MB of pictures runs
in release only.

**From the app's root** (also run):

```bash
bash scripts/build_libs.sh host --only pdf          # Linux: builds only libpdf_ffi.so (release)
eval "$(bash scripts/build_libs.sh env --only pdf)"  # exports PDF_CORE_LIB_PATH
flutter test test/services/pdf_core_native_test.dart \
  test/services/pdf_document_pin_test.dart \
  test/services/pdf_workflow_native_test.dart          # 17 passed
```

The app's full suite (`flutter test`, about 700 tests) includes the PDF
viewer tests and `test/pvs_pdf_test.dart`, which sweeps the corpus.

**`scripts/build_libs.sh --only pdf`** (the app's script for this engine) takes
`android`, `ios`, `macos`, `host` and `test`; add `--with-pdf` to build the
PDF engine together with the other engines. Its `test` runs
`cargo test --workspace --release`. `ios` and `macos` need a Mac.

**Android.**
- `pdf_ffi/build.rs` links the Android `cdylib` with
  `-z max-page-size=16384` and `-z common-page-size=16384` (16 KB pages).
- Needs `cargo-ndk`, the targets `aarch64-linux-android`,
  `armv7-linux-androideabi` and `x86_64-linux-android`, and an NDK. On this
  machine all are installed; the NDK is
  `/home/user/android-ndk/android-ndk-r27c` (r27c, which the app's guide
  names).
- `ANDROID_NDK_HOME=<ndk> bash scripts/build_libs.sh android --only pdf` (from the
  app) runs `cargo ndk` for all three ABIs with
  `--locked --release` **into the committed `android/src/main/jniLibs/`**.
  Run it when Rust that ships has changed, and commit the binaries with the
  source.
- To check a build without touching the committed files, leave out `-o`:
  `ANDROID_NDK_HOME=<ndk> cargo ndk -t arm64-v8a build --locked -p pdf_ffi --release`
  (run from `rust/`). It wrote `rust/target/aarch64-linux-android/release/libpdf_ffi.so`,
  byte-identical to the committed arm64 binary.
- Check 16 KB pages with the NDK's `llvm-readelf -lW <lib>`: every `LOAD`
  segment shows alignment `0x4000` (all three committed binaries do), or
  with the app's `python3 scripts/check_android_native.py <apk>`.
- The app's Gradle preflight (`verifyEngines`) checks only the per-format
  engines. Nothing stops a stale PDF binary, so rebuild it yourself.

**Apple.** `bash scripts/build_libs.sh ios macos --only pdf` from the app, on the
Mac. The committed macOS and iOS binaries predate phase 8 (PLAN.md), so
they lack the phase 8 functions until rebuilt there.

**Rust version.** The vendored codecs declare `rust-version = "1.92"`, so
this workspace needs Rust 1.92 or newer (the app's guide says 1.88 for the
other engines).

**Clippy.** With cargo 1.97, clippy reports 35 warnings in the engine's
crates (lints such as `manual_is_multiple_of`, `needless_range_loop`,
`too_many_arguments`) and 16 in vendored `hayro-jpeg2000`; with
`-D warnings` it stops in `pdf_core`. CI runs clippy as advisory. Leave
the vendored code alone, and do not add warnings in code you touch.

**Dart tests skip themselves** when `PDF_CORE_LIB_PATH` is unset
(`'set PDF_CORE_LIB_PATH to run'`). The pure-Dart tests still pass, so
such a run has tested less than it seems.

## FFI conventions here

This engine predates `doc_common` and keeps its own conventions; they
differ from the per-format engines' `doc_ffi` in places.

- **Exports** are `#[no_mangle] pub (unsafe) extern "C" fn pdf_*`, all in
  `rust/crates/pdf_ffi/src/api.rs`. There is no C code.
- **Strings** are UTF-8 and NUL-terminated. A returned `char*` belongs to
  the caller; free it with `pdf_free_string`. Byte buffers (renders) are
  freed with `pdf_free_buffer(ptr, len)`. `pdf_core_version` returns a
  static string; do not free it.
- **Failure** is `-1` (or NULL), and `pdf_last_error` returns a borrowed,
  thread-local `"CODE: message"`. The codes here are `ENCRYPTED`,
  `WRONG_PASSWORD`, `NOT_A_PDF`, `PAGE_OUT_OF_RANGE`, `ERROR`, `PANIC`,
  and `CANCELLED` for a render stopped by `pdf_render_cancel`.
  Parse, index, I/O and structure errors all come back as `ERROR`; the
  app-wide codes `IO`, `INVALID_ARGUMENT`, `BAD_HANDLE`, `UNSUPPORTED`,
  `CORRUPT`, `TOO_LARGE` and `NOT_FOUND` are not produced by this library.
  The Dart side adds `REGION_RENDER_UNAVAILABLE`, `OCR_UNAVAILABLE`,
  `TEXT_SELECTION_UNAVAILABLE` and `UNSUPPORTED` when a binary lacks a
  newer function, and `TIMEOUT` when an asynchronous render's `timeout`
  passed (it then cancels the render). A crafted file that cannot be read
  is `ERROR`, never an abort.
- **Warnings.** Renders and `pdf_apply_edits` leave newline-separated
  warnings in the thread-local `pdf_last_warnings` (what was approximated
  or left out; characters a standard font lacks). Read them right after
  the call, on the same thread.
- **Panics** are caught (`catch_unwind` in `run_str`, `run_int` and the
  render wrappers) and reported as `PANIC: internal error`. Allocation
  aborts, stack overflows and out-of-memory kills cannot be caught, so
  counts read from a file are checked before anything is allocated for
  them, recursion is capped, and work and memory are bounded (the limits
  below).
- **No session `Registry`.** Calls take a path and a password, and each one
  opens and parses the file. A caller that keeps reading one document pins
  it with `pdf_document_open` / `pdf_document_close` (Dart
  `openDocument`/`closeDocument`): read-only calls on the same path and
  password then share one parse, reference-counted, for as long as the
  file's length and modification time are unchanged. Writing calls always
  parse afresh.
- **Structured data crosses as JSON** (serde, camelCase; option structs
  use `deny_unknown_fields`).
- **Pages.**
  - 0-based: the render functions and `pdf_page_size`.
  - 1-based: `pdf_page_text_layout_json`, `pdf_ocr_page_json`,
    `pdf_annotations_json`, pages inside form, edit and OCR JSON, and
    range strings (`"1-3,5"`; empty means every page).
  - `pdf_extract_text`: 0 means every page, joined by form feeds (U+000C);
    otherwise a 1-based page.
- **Text offsets** in the text layout are UTF-16 code units, end exclusive.
- **Positions** are displayed page points with a top-left origin, with the
  CropBox origin and `/Rotate` applied (text layout, OCR, forms,
  annotations, edits).
- **Dart.** `PdfCore` has synchronous methods and `...Async` variants that
  run them in `Isolate.run`; `pageTextLayout`, `ocrPage`, `openDocument`
  and `closeDocument` are always asynchronous. Heavy calls (rendering,
  text, OCR, edits, compression) belong off the UI isolate. The error and
  warnings are read inside the same isolate as the call; keep it so.
- **Loading.** `PDF_CORE_LIB_PATH` overrides everything. Otherwise
  `libpdf_ffi.so` on Android and Linux; on iOS and macOS
  `DynamicLibrary.process()`, then `executable()` (iOS force-loads the
  static XCFramework, macOS vendors the dylib through its podspec);
  `pdf_ffi.dll` on Windows.
- **New exports** go in `api.rs`, `bindings.dart` (as an optional lookup,
  so older binaries still load), `pdf_core_api.dart` with an `Async`
  variant, and the exports of `flutter_pdf_core.dart`, with tests on both
  sides and a `CHANGELOG.md` entry.

**Main exported functions, by purpose:**

| Purpose | Functions |
| --- | --- |
| Library | `pdf_core_version`, `pdf_last_error`, `pdf_last_warnings`, `pdf_free_string`, `pdf_free_buffer` |
| Pinning | `pdf_document_open`, `pdf_document_close` |
| Inspecting | `pdf_page_count`, `pdf_inspect_json`, `pdf_get_metadata_json`, `pdf_page_size` |
| Pages (write a new file) | `pdf_extract_pages`, `pdf_delete_pages`, `pdf_reorder_pages`, `pdf_merge`, `pdf_rotate_pages`, `pdf_set_crop_box`, `pdf_set_metadata` |
| Passwords | `pdf_encrypt` (AES-256), `pdf_decrypt` |
| Text | `pdf_extract_text`, `pdf_page_text_layout_json`, `pdf_export_ai` |
| Rendering | `pdf_render_page_png`, `pdf_render_page_rgba`, `pdf_render_page_region_png`, `pdf_render_page_region_rgba`; each has a `..._cancellable` twin taking a render id, which `pdf_render_cancel` stops |
| Making PDFs | `pdf_images_to_pdf` (JPEG files) |
| OCR | `pdf_ocr_page_json`, `pdf_make_searchable_json`, `pdf_apply_ocr_json` |
| Forms, annotations, marks | `pdf_form_fields_json`, `pdf_annotations_json`, `pdf_apply_edits` |
| Smaller copies, pictures | `pdf_compress_json`, `pdf_page_images_json` |

## Engine conventions and pitfalls

- **Passwords and encryption.** Encrypted files are decrypted at load and
  written decrypted. So the rewriting operations (split, delete, reorder,
  merge, rotate, crop, metadata, making searchable) drop the password and
  invalidate digital signatures. `pdf_apply_edits` keeps both (an
  incremental update, encrypted with the file's own key), and compression
  encrypts again with the password. `pdf_encrypt` writes AES-256 (R6)
  only, and the owner password grants every permission. Passwords are
  encoded as each revision requires (PDFDocEncoding for RC4 and AES-128,
  SASLprep then UTF-8 for AES-256), with raw UTF-8 as a fallback.
- **Edits** (`pdf_edit`). Saves append an incremental update, with a
  cross-reference stream when the file uses them; a file whose index had
  to be rebuilt is written whole. Every annotation and filled field gets
  an appearance stream. Filling a form that also has XFA removes the XFA,
  so readers show the AcroForm values. Text is set in the standard 14
  fonts (WinAnsi): other characters, Indic scripts among them, come out as
  "?" with a warning. `flatten` draws an item into the page.
- **Split and merge** keep AcroForm fields and bookmarks, rename colliding
  fields on merge, and refuse XFA forms.
- **Damaged files.** The index is rebuilt by scanning when it is broken;
  password checks still apply, and render warnings say the file was
  recovered. Common writer quirks (offset 0 entries, a missing `endobj`,
  overflowing object numbers) read as intact files.
- **Rendering** returns warnings instead of failing when it approximates
  (an unreadable codec, a substituted font, XFA, missing appearances, a
  resource limit). Viewers use the warning-aware calls. Output follows
  PDFium's conventions (area-sampled images, rectangles snapped to whole
  pixels, strokes at least one pixel). Pages are drawn in bands on up to 4
  threads and stay pixel-identical to a one-thread render.
- **Limits in force:**

  | Limit | Value | Where |
  | --- | --- | --- |
  | Decoded stream | 128 MiB | `pdf_core/src/filter.rs` |
  | Object nesting | 128 | `pdf_core/src/parser.rs` |
  | `/Prev` chain | 64 | `pdf_core/src/xref.rs` |
  | Arrays and dictionaries nested in a content stream | 64 | `pdf_text/src/content_stream.rs` |
  | Form XObjects and Type 3 glyphs nested while rendering | 12 deep | `pdf_render/src/page_forms.rs`, `page.rs` |
  | Object stream header | `/N` at most (`/First` + 1) / 4; objects read at most 4 times the decoded stream | `pdf_core/src/document.rs` |
  | Recovery work | 8 times the file size (at least 1 MiB) for sections, and as much for stream lengths read through the index | `pdf_core/src/recovery.rs` |
  | Render time | 20 s (`RenderOptions::time_limit`, `DEFAULT_TIME_LIMIT`); Dart's asynchronous renders time out at 30 s | `pdf_render/src/page.rs`, `lib/src/pdf_core_api.dart` |
  | Render work, per band | 50M units (an operator or a glyph), 50,000 form XObjects, 128 MiB of nested parsed content, 1M glyphs a string | `pdf_render/src/page_limits.rs` |
  | Parsed content | 128 MiB a stream; 64 operands an operator; 1M values in one operator's arrays | `pdf_text/src/content_stream.rs` |
  | Type 3 procedures kept | 4 MiB a font (16 MiB one procedure) | `pdf_render/src/font/mod.rs` |
  | Decoded pictures | 512 MiB a page, each picture counted once a render | `pdf_render/src/image.rs`, `page_images.rs` |
  | Text extraction, per page | 50M operators, 50,000 forms, 1M glyphs, 256 saved states, 256 MiB of content | `pdf_text/src/extractor.rs` |
  | Render output | 32M pixels (`RenderOptions::max_pixels`) | `pdf_render/src/page.rs` |
  | Image | 64M pixels | `pdf_render/src/image.rs` |
  | JPEG 2000 samples | 32M | `pdf_render/src/image_codecs.rs` |
  | Offscreen layers and masks | 128 MiB | `pdf_render/src/page_limits.rs` |
  | Tiling-pattern work | 256M (tiles times canvas pixels) | `pdf_render/src/page_limits.rs` |
  | OCR page | 12M pixels, 50,000 words | `pdf_ocr/src/pdf.rs` |
  | Edit pictures | 8192 px a side | `pdf_edit/src/image.rs` |
  | Vendored codecs | see `rust/vendor/README.md` | keep these changes when upgrading |

- **Past a limit**, a render stops drawing, keeps its pixels and warns
  "page rendering stopped: page too complex to draw in full" (that warning
  comes first, as a viewer shows only the first); a picture past the page's
  budget is skipped with a warning; text extraction returns the text read
  so far. Pages as pictures (`page_image`) and OCR inherit the render time
  limit, and `pageImages` has no warnings channel, so a page stopped there
  is saved as drawn.
- **OCR** reads printed Latin-script text (208 characters) only. Pages
  that already have text are skipped unless `force` is set. The searchable
  copy is not encrypted.
- **Generated data.** `pdf_text/data/cmaps.bin` comes from the
  `build_cmap_bundle` example, `pdf_render/data/fallback-fonts.bin` from
  `build_fallback_bundle`, and `pdf_ocr/models/latin.ocrm` from
  `tools/ocr_train`. `pdf_ocr/tests/golden.rs` holds the Rust
  normalization and network to the Python originals.
- **The CLI writes with `std::fs::write`** (`edit`, `compress`, `images`).
  It is a developer tool; FFI code saves atomically.
- **Error codes.** Phase 10's plan names `CORRUPT` for a crafted file;
  this library reports it as `ERROR` (`PdfError::Structure`), which the
  Dart side and the app already handle, and its tests check for that.
- **Object numbers.** A file may number an object 4294967295. New objects
  then take the lowest free number (`PdfDocument::add_object`), compression
  refuses, and `pdf_edit` refuses an update that would need numbers past
  it, rather than wrap onto the file's own objects.
- **Stale notes.** `docs/ARCHITECTURE.md` still calls the FFI stateless
  (pinning came later) and says CI denies clippy warnings (it is advisory);
  the roadmap in `docs/MILESTONES.md` lists work done since; and
  `docs/last_session.txt` describes an OCR model that has since been
  retrained. Trust the code, `README.md` and `CHANGELOG.md` first.

## Where work stands and what is next

- Phases 0 to 9 of the app are done. Phase 8 added `pdf_edit` and
  `pdf_compress`, and the Android binaries were rebuilt then (last commit
  on 2026-10-13). The macOS and iOS binaries need rebuilding on the Mac.
- **Phase 10, batch B3** in the app's `docs/PHASE10_PLAN.md` (wave 1, size
  L, two agents) hardened this engine, and is done (the limits above, and
  `CHANGELOG.md`). Left for later: the macOS and iOS binaries (the Mac),
  `pdf_ocr/src/layer.rs` still holding every operation of a page with
  `parse_content`, and a deadline for text extraction. What it did:
  - `pdf_core`: give `extract_from_object_stream` (`document.rs`) the
    guard `extract_all_from_object_stream` has and stop reserving
    `Vec::with_capacity(n)` from the file (a crafted `/N` aborts with a
    32 GB request); parse an object stream's header once and cache it per
    container (N = 32,000 takes over 120 s); remember parsed xref sections
    by offset and bound recovery by file size (a 1 MB file takes 17 s);
    replace the `resolve().unwrap()` chain in `pdf_ops/src/preserve.rs`.
  - `pdf_render`, `pdf_text`, `pdf_ffi` and Dart: a render work budget with
    a deadline and a cancel call keyed by render id, through
    `run_operations`, `draw_form`, `draw_type3` and tiling, ending in a
    "page too complex" warning (the app's `lib/services/pdf_raster.dart`
    then shows a placeholder); text extraction on the compact `Content`
    form or with an operator cap (5.6 GB today); a budget for decoded image
    bytes per page.
  - Verify: `cargo test --workspace --release`, hostile builders in Rust
    tests (the huge-`/N` file fails cleanly, k=3 and k=4 form bombs return
    within the deadline, the 60M-operator page extracts under 500 MB,
    N = 32,000 opens and M = 1000 recovers in under 1 s), the package's and
    the app's PDF viewer tests, then rebuild the committed Android binaries
    with NDK r27c.
- **Later batches:** B9 adds a corpus sweep and in-tree xorshift mutation
  fuzzing for this engine (xref and recovery, filter chains, fonts,
  codecs), with its own copy of `doc_testkit` because it does not depend on
  `doc_common`; deadlines and memory limits in `test/pvs_pdf_test.dart`;
  every hostile suite again with `RUST_MIN_STACK=524288`. B10 autosaves
  `pdf_edit`'s pending items as drafts. `cargo-fuzz` and `proptest` are not
  allowed.
- **Hostile checks on this machine:** stack with
  `std::thread::Builder::stack_size` at 512 KiB and 1 MiB, hangs under
  `timeout`, memory under `prlimit --as=...` or from `VmHWM` in
  `/proc/self/status`.
- **Owner decisions that touch this engine:** about 20 s of rendering per
  page before "page too complex", Crashlytics NDK and native symbols, and
  whether the release profile keeps line tables.
- **After release:** embedded fonts for text in form fields and text boxes,
  so Indic scripts stop coming out as "?".
- **2026-10-19 (uncommitted):** the release profile keeps line tables
  (`debug = "line-tables-only"`, for Crashlytics NDK), so the committed
  `jniLibs` grow when next rebuilt; `pdf_ffi/tests/mutate.rs` is a
  self-contained mutator, not yet run.
