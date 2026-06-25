# PDF Viewer Utility

A small non-Flutter desktop utility for opening PDFs on macOS and Windows.

It starts a local-only web UI, opens it in your default browser, serves the PDF
through `flutter_pdf_core`'s Rust writer, and uses the library for document
inspection and page text extraction.

```bash
cargo run --manifest-path tools/pdf_viewer/Cargo.toml -- test/sample.pdf
```

Build a standalone executable:

```bash
cargo build --manifest-path tools/pdf_viewer/Cargo.toml --release
```

The built binary is at `tools/pdf_viewer/target/release/pdf_viewer` on macOS
and `tools/pdf_viewer/target/release/pdf_viewer.exe` on Windows.

Options:

```text
pdf_viewer [pdf-path] [--password <password>] [--port <port>] [--no-open]
```
