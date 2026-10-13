/// flutter_pdf_core — a lightweight PDF toolkit built from scratch in Rust.
///
/// Parse, inspect, split, merge, reorder, rotate, crop, edit metadata,
/// extract text, recognize scanned text (OCR) and make scans searchable, fill
/// forms, annotate, sign and stamp pages, render
/// pages to pixels, build PDFs from images, export AI-ready JSON/NDJSON, and
/// add/remove AES-256 passwords — all on-device
/// with no third-party PDF dependency.
library flutter_pdf_core;

export 'src/pdf_edit.dart';

export 'src/pdf_core_api.dart'
    show
        PdfCore,
        PdfException,
        PdfImageFit,
        PdfInfo,
        PdfMetadata,
        PdfOcrLine,
        PdfOcrOptions,
        PdfOcrPage,
        PdfOcrPageReport,
        PdfOcrStatus,
        PdfOcrWord,
        PdfPageSize,
        PdfSearchableReport,
        PdfPageTextLayout,
        PdfTextGlyph,
        PdfRenderedPage,
        PdfRenderedPng;
