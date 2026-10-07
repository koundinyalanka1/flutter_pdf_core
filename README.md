# flutter_pdf_core

A lightweight PDF toolkit for Flutter, **built from scratch in Rust** — no third-party PDF library anywhere in the stack.

* Parse + inspect (classic xref, xref streams, incremental updates, object streams, FlateDecode)
* Conservative damaged-index recovery, retaining password authentication
* Split, merge, delete, reorder, duplicate pages
* Preserve AcroForm fields and retained-page bookmarks during split/merge
* Rotate, crop, edit document metadata
* Text extraction (encodings, ToUnicode CMaps, CID fonts)
* Positioned text and glyph geometry for selection/search
* CPU raster rendering to PNG or RGBA, with warnings for approximate/skipped content
* JPEG-to-PDF composition with page-size, fit and margin controls
* AI-ready JSON/NDJSON export (paragraph-aware chunks with overlap) for feeding local models
* AES-256 password protection; opens RC4/AES-128/AES-256 encrypted files
* Hand-written C ABI + `dart:ffi` — no codegen, no heavy runtime, small binaries

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
```

## Project layout

| Crate | Purpose |
|---|---|
| `rust/crates/pdf_core` | lexer, parser, object model, xref (+streams), filters, writer, crypto |
| `rust/crates/pdf_ops` | page tree, split/delete/reorder, merge, rotate/crop, metadata |
| `rust/crates/pdf_text` | content streams, fonts, text extraction |
| `rust/crates/pdf_render` | CPU rasterizer, fonts, images, paints, annotations |
| `rust/crates/pdf_ai` | chunking + JSON/NDJSON export for local AI models |
| `rust/crates/pdf_ffi` | the C ABI consumed by `dart:ffi` |
| `rust/crates/pdf_cli` | developer CLI |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for design notes and
[docs/MILESTONES.md](docs/MILESTONES.md) for the milestone-by-milestone log,
known limitations and the roadmap.
