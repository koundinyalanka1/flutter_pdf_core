# flutter_pdf_core

A lightweight PDF toolkit for Flutter, **built from scratch in Rust** — no third-party PDF library anywhere in the stack.

* Parse + inspect (classic xref, xref streams, incremental updates, object streams, FlateDecode)
* Split, merge, delete, reorder, duplicate pages
* Rotate, crop, edit document metadata
* Text extraction (encodings, ToUnicode CMaps, CID fonts)
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
```

Heavy calls have `...Async` variants that run on a background isolate. Errors throw `PdfException` with stable codes (`ENCRYPTED`, `WRONG_PASSWORD`, …). Page selections are 1-based range strings like `'1-3,5'`.

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
| `rust/crates/pdf_ai` | chunking + JSON/NDJSON export for local AI models |
| `rust/crates/pdf_ffi` | the C ABI consumed by `dart:ffi` |
| `rust/crates/pdf_cli` | developer CLI |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for design notes and
[docs/MILESTONES.md](docs/MILESTONES.md) for the milestone-by-milestone log,
known limitations and the roadmap.
