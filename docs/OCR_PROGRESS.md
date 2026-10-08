# OCR: progress and next steps

Status on 2026-10-08. The work is uncommitted on `main`.

## Done

The engine, its tests and its API are complete. The model works but was trained for only 40% of the planned run.

| Area | Where | State |
|---|---|---|
| OCR engine, from scratch | `rust/crates/pdf_ocr` | ✅ Done. Binarization, components, skew and orientation (sideways and upside down), lines, reading order, CNN + BiLSTM recognizer with CTC, words and boxes. |
| Invisible text layer | `pdf_ocr::layer` | ✅ Done. Renders pixel-identically, gives exact word boxes, and works with `/Rotate` and in Apple PDFKit. |
| PDF integration | `pdf_ocr::pdf` | ✅ Done. Skips pages that already have text, renders at 300 dpi within a 12 MP budget, and writes layers. |
| C ABI | `pdf_ffi` | ✅ `pdf_ocr_page_json`, `pdf_make_searchable_json`, `pdf_apply_ocr_json` |
| Dart API | `lib/src/pdf_core_api.dart` | ✅ `ocrPage`, `makeSearchable(Async)`, `applyOcr(Async)`, `PdfOcr…` types |
| CLI | `pdf_cli` | ✅ `ocr`, `searchable`, `ocr-eval`, `ocr-image`. Set `PDF_OCR_MODEL=<file>` to try a model without rebuilding. |
| `pdf_text` fixes | `extractor.rs`, `text_state.rs` | ✅ Rotation-aware plain-text breaks; text-render-mode stats |
| Training tools | `tools/ocr_train` | ✅ Data download, generator, model, training, export, golden data, evaluation PDFs |
| Docs | README, ARCHITECTURE, MILESTONES (M14), CHANGELOG, `tools/ocr_train/README.md`, `pdf_ocr/models/README.md` | ✅ Updated |

## Measured

* **Accuracy:** 1.45% CER and 8.8% WER on 72 pages typeset in 24 macOS fonts
  the model never saw, at 300 dpi. Results ranged from 0.5% (Times New Roman,
  Trebuchet) to 4% (American Typewriter).
* **Reference-text errors:** about 20% of the counted errors are not OCR
  mistakes. OCR correctly reads "fi" and "fl" where the PDFs' own extracted
  text has "Þ" and "ß" (see the bugs below).
* **Speed:** about 0.1–0.3 s per page on an Apple M4 Pro using all cores, and
  about 1.7 s on one core.
* **Size:** the Android arm64 `libpdf_ffi.so` grows from 2.83 MB to 4.23 MB
  (stripped), of which about 1 MB is the model.
* **Tests:** `cargo test` passes for `pdf_ocr` (17 unit, 2 golden, 1
  orientation, 4 end-to-end) and for `pdf_ffi`, `pdf_text` and `pdf_ai`.
  `flutter test` passes, and `flutter analyze` reports no new issues.

## Not done yet, in order

1. **Finish training.**
   * The bundled model is the step-32,000 checkpoint of an 80,000-step run.
   * The run's files were in a temporary session directory and may be gone.
     If so, rerun `tools/ocr_train/prepare_data.sh` and `train.py` from
     scratch (about 2 hours on an M4 Pro). Otherwise use
     `train.py --resume <run>/last.pt`.
   * Then run `export.py --golden …`, which rewrites the model and the golden
     fixtures, followed by `cargo test -p pdf_ocr` and `pdf_cli ocr-eval`.
   * Update the accuracy line in the README and in `models/README.md`.
2. **Rebuild the checked-in native binaries** (`scripts/build_android.sh`,
   `build_ios.sh`, `build_macos.sh`). They do not contain OCR yet, so the
   Dart calls throw `OCR_UNAVAILABLE`. Cross-compiling for Android arm64 and
   iOS has been checked and works.
3. **Run clippy.** It is not installed locally; CI runs it.
4. **Run the on-device integration tests** for OCR (not run).
5. **Model improvements.** The fixes below need retraining.
   * Underscores: `textgen.py` strips `_` from the corpus, so `pdf_ops` reads
     as `pdf ops`.
   * More `I`/`l`/`1` context.
   * More typewriter faces.

## Bugs found along the way (pre-existing, not fixed)

* `pdf_text/src/font.rs`: the MacRoman table lacks 0xDE→ﬁ and 0xDF→ﬂ (and
  other entries), so "file" extracts as "Þle" (see `test/sample.pdf`).
* `PdfWriter` atomic saves create files with `0600` permissions (inherited
  from `tempfile`).

## Known limitations

* Printed Latin-script text only; no handwriting.
* Tables read column by column.
* No text inside photographs or white-on-black areas.
