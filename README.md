# flutter_pdf_core

A lightweight PDF toolkit for Flutter, **built from scratch in Rust** — no third-party PDF library anywhere in the stack.

* Parse + inspect (classic xref, xref streams, incremental updates, object streams, FlateDecode)
* Conservative damaged-index recovery, retaining password authentication
* Split, merge, delete, reorder, duplicate pages
* Preserve AcroForm fields and retained-page bookmarks during split/merge
* Rotate, crop, edit document metadata
* Text extraction (encodings, ToUnicode CMaps, CID fonts)
* Positioned text and glyph geometry for selection/search
* OCR for scanned pages, also built from scratch: reads printed Latin-script text,
  makes scans searchable with an invisible text layer, and gives scans the same
  selection/search geometry as born-digital pages
* CPU raster rendering to PNG or RGBA, with warnings for approximate/skipped content;
  whole pages or just a region of one (for sharp zoom without huge bitmaps)
* Fill forms (text, check boxes, radio buttons, lists), annotate (highlight,
  underline, strike-out, ink, shapes, text boxes, notes), sign with a drawn or
  pictured signature, and draw page numbers, headers and watermarks onto pages,
  saved as an incremental update that keeps signatures valid and passwords on
* Compress: pictures scaled to a chosen resolution and re-encoded with the
  library's own JPEG encoder, streams packed, the file written with object
  streams; pages saved as PNG or JPEG pictures
* JPEG-to-PDF composition with page-size, fit and margin controls
* AI-ready JSON/NDJSON export (paragraph-aware chunks with overlap) for feeding local models
* AES-256 password protection; opens RC4/AES-128/AES-256 encrypted files
* Hand-written FFI layer in Rust (`extern "C"` exports, the calling convention `dart:ffi` uses; no C code) — no codegen, no heavy runtime, small binaries

## Quick start (Dart)

```dart
import 'package:flutter_pdf_core/flutter_pdf_core.dart';

final info = PdfCore.inspect('/path/in.pdf');          // version, pages, metadata
PdfCore.merge(['/a.pdf', '/b.pdf'], '/merged.pdf');
PdfCore.extractPages('/merged.pdf', '1-3,7', '/subset.pdf');
PdfCore.rotatePages('/subset.pdf', 90, '/rotated.pdf');
PdfCore.setMetadata('/rotated.pdf', PdfMetadata(title: 'Report'), '/final.pdf');
PdfCore.encrypt('/final.pdf', 'user-pw', '/locked.pdf');

final text = await PdfCore.extractTextAsync('/final.pdf');
final ndjson = await PdfCore.exportForAiAsync('/final.pdf', ndjson: true);

// Raster page indexes are 0-based; text-layout page numbers are 1-based.
final rendered = await PdfCore.renderPagePngWithWarningsAsync(
  '/final.pdf', 0, width: 1200,
);
// Display rendered.bytes and surface rendered.warnings to the reader.
final layout = await PdfCore.pageTextLayout('/final.pdf', page: 1);

// Scanned pages: read one for a viewer, or make a whole file searchable.
final ocr = await PdfCore.ocrPage('/scan.pdf', page: 1);  // ocr.text, ocr.layout
await PdfCore.makeSearchableAsync('/scan.pdf', '/scan-searchable.pdf');

await PdfCore.imagesToPdfAsync(
  ['/photo1.jpg', '/photo2.jpg'], '/photos.pdf',
  fit: PdfImageFit.contain,
  pageSizePoints: PdfPageSize(595, 842),
  margin: 24,
);
```

Heavy calls have `...Async` variants that run on a background isolate. Errors throw `PdfException` with stable codes (`ENCRYPTED`, `WRONG_PASSWORD`, …). Page selections are 1-based range strings like `'1-3,5'`.

Every call reads and parses its file afresh. A screen that keeps reading one
document (a viewer rendering pages, measuring them and loading text at once)
should pin it, so read-only calls share one parse while the file is unchanged:

```dart
final pages = await PdfCore.openDocument('/big-scan.pdf', password: pw);
// ... renders, page sizes, text and text layout reuse that parse ...
await PdfCore.closeDocument('/big-scan.pdf', password: pw);
```

## Fidelity and recovery

Rendering supports shading types 1–7, sampled/calculator colour functions,
coloured/uncoloured tiling patterns, standard blend modes, transparency groups,
alpha/luminosity masks, inline images and annotation appearance streams. Image
codecs include JPEG, JPEG 2000, JBIG2 (shared globals), CCITT, Flate and LZW.
Standalone image decoders are used; the PDF parser and renderer remain native
project code. Vendored codec allocation limits and licenses are documented in
[rust/vendor/README.md](rust/vendor/README.md).

Screen output follows PDFium's conventions where they make pages read more
clearly: images are area-sampled, lone filled rectangles and rectangular
clips land on whole pixels, and no stroke is thinner than one device pixel.
To look as sharp as other viewers, render at the page's size on screen in
device pixels (its width, not its long edge in a square box), and draw the
bitmap one pixel per device pixel.

Missing annotation appearances can use geometry/text/widget fallbacks including
rotation, comb cells, choice selection and button captions/icons. Generated
artwork remains approximate and warns. XFA, rich-text fallback styling, cloudy
borders and display-dependent NoZoom are not fully supported. Advanced blending
colour spaces are converted to RGB with warnings; shading currently accepts
DeviceGray/RGB/CMYK. Damaged inputs and bounded-resource limits also warn. Use the
warning-aware render API in a viewer. This is not universal PDF compatibility.

Split and merge preserve AcroForm field trees and remap bookmarks to the pages
that remain. Merge renames colliding form fields. XFA forms are rejected with
an explicit error because they cannot be safely pruned by these operations.
JavaScript strings referring to renamed field names are not rewritten.
Rewriting a signed PDF invalidates its digital signatures; displaying a
signature's appearance does not verify its validity.

If an index is damaged, the reader tries to recover complete objects and
surviving trailer information. Password checks still apply. Render warnings
identify recovered files, which may be incomplete; keep the original.
Recovery cannot recreate missing content from a truncated download. Common
writer quirks are not damage: index entries in use at offset 0, `endobj`
omitted before the next object or the index, and overflowing object numbers
(references to them read as null) are read as intact files.

Encryption currently grants all document permissions. The optional owner
password also unlocks the file; it does not enable printing/copying restrictions.
Pass passwords as typed: they are encoded as each revision requires
(PDFDocEncoding for RC4/AES-128, UTF-8 after SASLprep for AES-256), with the
raw UTF-8 bytes as a fallback.

## OCR

A scanned page holds a picture of text, not text. `ocrPage` reads a page.
`makeSearchable` saves a copy in which each scanned page carries its
recognized words as invisible text over the glyphs, so selection, search,
`extractText` and `exportForAi` work on it, here and in other viewers.
`ocrPage` also returns the page's selectable layout, the same shape
`pageTextLayout` gives, so a viewer can select and search a scan right away.
To show progress on a long document, recognize pages one at a time and write
them with `applyOcr`. Results from another engine can be written the same way.

```dart
final page = await PdfCore.ocrPage('/scan.pdf', page: 1);
print('${page.text} (confidence ${page.confidence})');
final report = await PdfCore.makeSearchableAsync('/scan.pdf', '/out.pdf');
```

The engine is part of this library, written from scratch in Rust like the
rest of it:

* **Finding the text:** adaptive binarization, connected components, skew
  correction, sideways and upside-down page detection, line finding and
  reading order (columns before rows).
* **Reading it:** a small neural network trained for this library, a CNN and a
  bidirectional LSTM decoded with CTC. It has about half a million weights,
  is compiled in, and adds about 1 MB.

No OCR or machine-learning library runs on the device.
[`tools/ocr_train`](tools/ocr_train) reproduces the model from open-licensed
fonts and public-domain text.

* Reads printed Latin-script text: English and Western European languages,
  digits, and common document punctuation (208 characters). Handwriting and
  other scripts are not supported.
* Accuracy: 0.30% character error rate (1.8% word error rate) on 72 pages
  typeset in 24 macOS fonts the model never trained on, rendered at 300 dpi.
  Most fonts score 0.1–0.3%, and none is worse than 0.9%.
  A page takes about 0.1–0.3 s on an Apple M4 Pro (all cores) and about
  1.7 s on one core.
* Pages that already carry text, born-digital or from earlier OCR, are left
  alone unless `force` is set. The layer renders invisibly, and its word boxes
  match the scanned words, also on pages with `/Rotate`.
* Saving rewrites the file, like the other writing operations. The copy is not
  encrypted, and digital signatures on the original no longer verify.
* Not yet handled: text inside photographs, white-on-black text, and tables,
  which read column by column.

## Filling, annotating and stamping

`applyEdits` takes everything one save changes: field values, annotations to
add or remove, and marks drawn onto pages. Positions are displayed page points
with a top-left origin, as `pageTextLayout` gives them, so a selection becomes
a highlight as it is, also on pages with `/Rotate`.

```dart
final form = await PdfCore.formFieldsAsync('/form.pdf');
final warnings = await PdfCore.applyEditsAsync(
  '/form.pdf',
  PdfEdits(
    fields: [PdfFieldValue.text('name', 'Ada'), PdfFieldValue.checked('agree', true)],
    annotations: [
      PdfAnnotationEdit.markup(PdfAnnotationType.highlight,
          page: 1, rects: [PdfBox(72, 100, 300, 114)]),
      PdfAnnotationEdit.note(page: 1, at: (500, 40), text: 'Check this'),
    ],
    marks: [PdfPageMark.pageNumbers(format: 'Page {page} of {pages}')],
  ),
  '/form.pdf',
);
```

* The original bytes are kept and the changes appended as an incremental
  update (with a cross-reference stream when the file uses them), so existing
  digital signatures still verify and nothing the engine does not understand is
  lost. A file whose index had to be rebuilt is written out whole instead.
* An encrypted file stays encrypted: what the update adds is encrypted with the
  file's own key (RC4, AES-128 or AES-256).
* Every annotation, and every filled field, gets an appearance stream, so any
  reader shows it as the app drew it. Filling a form that also has an XFA
  description removes the XFA, so readers show the values written here.
* Text is set in the standard PDF fonts (WinAnsi); characters outside them are
  written as "?" and reported in the warnings.
* `flatten` draws an annotation into the page instead, where it can no longer
  be moved or removed.

## Building the native core

```bash
cd rust && cargo test --workspace          # run the full native test suite

./scripts/build_android.sh                 # → android/src/main/jniLibs/**.so
./scripts/build_ios.sh                     # → ios/Frameworks/PdfFfi.xcframework
./scripts/build_macos.sh                   # → macos/Frameworks/libpdf_ffi.dylib
```

Android needs `cargo install cargo-ndk` + NDK; Apple targets need the respective `rustup target add` (see script headers). Run the matching script before building the Flutter app for that platform.

There is also a developer CLI for poking at real files:

```bash
cargo run -p pdf_cli -- inspect some.pdf
cargo run -p pdf_cli -- text some.pdf
cargo run -p pdf_cli -- encrypt some.pdf user-pw owner-pw locked.pdf
cargo run --release -p pdf_cli -- ocr scan.pdf                  # recognized text
cargo run --release -p pdf_cli -- searchable scan.pdf out.pdf   # invisible text layer
cargo run --release -p pdf_cli -- ocr-eval text.pdf             # accuracy vs. real text
cargo run --release -p pdf_cli -- compress big.pdf small.pdf 150 70
cargo run --release -p pdf_cli -- images some.pdf page- jpeg 150  # page-1.jpg, page-2.jpg, …
cargo run --release -p pdf_cli -- fields form.pdf                # form fields as JSON
cargo run --release -p pdf_cli -- edit form.pdf edits.json out.pdf
```

## Project layout

| Crate | Purpose |
|---|---|
| `rust/crates/pdf_core` | lexer, parser, object model, xref (+streams), filters, writer, crypto |
| `rust/crates/pdf_ops` | page tree, split/delete/reorder, merge, rotate/crop, metadata |
| `rust/crates/pdf_text` | content streams, fonts, text extraction |
| `rust/crates/pdf_render` | CPU rasterizer, fonts, images, paints, annotations |
| `rust/crates/pdf_ai` | chunking + JSON/NDJSON export for local AI models |
| `rust/crates/pdf_ocr` | OCR: page analysis, recognizer inference, invisible text layers |
| `rust/crates/pdf_edit` | forms, annotations, signatures and page marks; incremental saves |
| `rust/crates/pdf_compress` | smaller copies (picture re-encoding, compact writing) and pages as pictures |
| `rust/crates/pdf_ffi` | the FFI layer (Rust, `extern "C"` exports) consumed by `dart:ffi` |
| `rust/crates/pdf_cli` | developer CLI |
| `tools/ocr_train` | offline training of the OCR model (Python, not shipped) |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for design notes and
[docs/MILESTONES.md](docs/MILESTONES.md) for the milestone-by-milestone log,
known limitations and the roadmap.
