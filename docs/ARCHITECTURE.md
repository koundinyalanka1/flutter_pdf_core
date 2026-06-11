# Architecture

```
┌────────────────────────────────────────────────────────────┐
│ Flutter app                                                │
│   lib/flutter_pdf_core.dart  →  PdfCore (Dart API)         │
│   lib/src/pdf_core_api.dart     sync + isolate-async       │
│   lib/src/bindings.dart         raw dart:ffi               │
└──────────────────────────┬─────────────────────────────────┘
                           │ C ABI (UTF-8 strings, int status,
                           │ pdf_last_error / pdf_free_string)
┌──────────────────────────┴─────────────────────────────────┐
│ rust/crates                                                │
│                                                            │
│  pdf_ffi   extern "C" surface, panic guards, range parsing │
│  pdf_cli   developer CLI (same ops, for manual testing)    │
│     │                                                      │
│  pdf_ai    chunker + JSON/NDJSON export        (M9)        │
│  pdf_text  content streams, fonts, extraction  (M8)        │
│  pdf_ops   page tree, split/merge/rotate/meta  (M3–M6)     │
│     │                                                      │
│  pdf_core  lexer, parser, object model, xref (+streams),   │
│            filters/Flate, writer, crypto       (M1,2,7,10) │
└────────────────────────────────────────────────────────────┘
```

## Design choices

**From scratch, lightweight.** No PDF crates. External crates are small, widely-audited primitives only: `flate2` (zlib), `aes`/`cbc`/`md-5`/`sha2` (RustCrypto), `getrandom`, `serde`/`serde_json`, `thiserror`. The FFI layer is hand-written instead of flutter_rust_bridge — zero codegen, zero extra runtime, smaller binaries.

**Immutable-ish core, explicit ops.** `PdfDocument` holds a `BTreeMap<ObjectId, IndirectObject>` plus the trailer. Operations in `pdf_ops` either mutate in place (`delete_pages`, `rotate_page`, `write_metadata`) and rely on `garbage_collect()`, or build fresh documents via the closure-copy engine in `split.rs` (used by both split and merge) which renumbers objects compactly and rewrites references.

**Decrypt at load, encrypt at save.** Encrypted input is decrypted into plain objects during `from_bytes_with_password` (strings + streams, object streams handled before expansion). The writer always emits decrypted output; `crypt::encrypt_to_bytes` produces an AES-256 (R6) protected file as an explicit step.

**Errors are typed.** `PdfError` covers parse/xref/filter/crypt/structure cases; the FFI maps them to stable codes (`ENCRYPTED`, `WRONG_PASSWORD`, `NOT_A_PDF`, `PAGE_OUT_OF_RANGE`, `ERROR`, `PANIC`) surfaced in Dart as `PdfException`.

**Stateless FFI.** Every native call opens the file, operates, saves, and returns. No cross-call handles, no lifetime bugs, trivially thread-safe; the OS page cache keeps repeat opens fast. If profiling ever shows this matters for huge documents, a handle-based API can be added without breaking the current one.

## Testing

* Unit tests live next to each module (`cargo test --workspace` — parser, xref-stream and `/Prev` chains, predictors, page-tree inheritance, split/merge round-trips, text extraction against synthetic content streams, chunker invariants, RC4/AES vectors, full encrypt→reopen cycles, and an end-to-end FFI smoke test through the real C ABI).
* Dart unit tests cover the pure-Dart layer; `example/integration_test` exercises the FFI on a device.
* CI (`.github/workflows/ci.yml`) runs fmt + clippy (warnings deny) + tests + an Android cross-build on every push.
