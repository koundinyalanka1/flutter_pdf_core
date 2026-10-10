# OCR: progress and next steps

Status on 2026-10-09. OCR is complete and shipped in the checked-in native
binaries. Work since the "partially trained ocr model" commit is uncommitted.

## Done

| Area | Where | State |
|---|---|---|
| OCR engine, from scratch | `rust/crates/pdf_ocr` | ✅ Binarization, components, skew and orientation (sideways and upside down), lines, reading order, CNN + BiLSTM recognizer with CTC, words and boxes |
| Model | `pdf_ocr/models/latin.ocrm` | ✅ Fully trained: 80,000 steps, best checkpoint at step 76,000 |
| Invisible text layer | `pdf_ocr::layer` | ✅ Renders pixel-identically, gives exact word boxes, works with `/Rotate` and in Apple PDFKit |
| PDF integration | `pdf_ocr::pdf` | ✅ Skips pages that already have text, renders at 300 dpi within a 12 MP budget, writes layers |
| C ABI | `pdf_ffi` | ✅ `pdf_ocr_page_json`, `pdf_make_searchable_json`, `pdf_apply_ocr_json` |
| Dart API | `lib/src/pdf_core_api.dart` | ✅ `ocrPage`, `makeSearchable(Async)`, `applyOcr(Async)`, `PdfOcr…` types |
| Native binaries | `android/`, `ios/`, `macos/` | ✅ Rebuilt with OCR; all three export the OCR functions |
| CLI | `pdf_cli` | ✅ `ocr`, `searchable`, `ocr-eval`, `ocr-image`. Set `PDF_OCR_MODEL=<file>` to try a model without rebuilding. |
| `pdf_text` fixes | `extractor.rs`, `text_state.rs`, `font.rs` | ✅ Rotation-aware plain-text breaks; text-render-mode stats; full MacRoman table (macOS fi/fl ligatures no longer come out as "Þ"/"ß") |
| Training tools | `tools/ocr_train` | ✅ Data download, generator, model, training on Apple or NVIDIA GPUs, export, golden data, evaluation PDFs |

## Measured

**Accuracy**, on 72 pages typeset in 24 macOS fonts the model never saw,
rendered at 300 dpi and scored with `pdf_cli ocr-eval`:

| Model and layout | CER | WER |
|---|---|---|
| Final model with the reading-order fix | **0.30%** | **1.8%** |
| Earlier partial model (step 32,000), same layout fix | 0.85% | 7.4% |
| Earlier partial model, earlier layout | 1.09% | 7.6% |

Most fonts score 0.1–0.3%. The weakest are Big Caslon (0.85%), Hoefler Text
(0.72%), Courier New (0.68%) and Menlo (0.61%).

**Other measurements:**
* **Speed:** about 0.1–0.3 s per page on an Apple M4 Pro using all cores,
  and about 1.7 s on one core.
* **Size:** the Android arm64 `libpdf_ffi.so` is 5.1 MB, up from 3.7 MB
  (unstripped as shipped). About 1 MB of that is the model.
* **Rust tests:** all suites pass, including `pdf_ocr`'s unit, golden,
  orientation and end-to-end tests and the OCR tests through the C ABI.
* **Dart tests:** all pass. `test/pdf_ocr_native_test.dart` runs OCR from
  Dart against a real native library when `PDF_CORE_LIB_PATH` is set. It
  passes with `macos/Frameworks/libpdf_ffi.dylib`.

## Still open

1. **Clippy.** Not installed locally; CI runs it.
2. **The example app's macOS build.** It fails before any test starts.
   * The committed Xcode project uses Swift Package Manager, and the plugin's
     `macos/flutter_pdf_core/Package.swift` depends on `../FlutterFramework`,
     which does not exist. This is pre-existing and not related to OCR.
   * The OCR integration test in `example/integration_test` is ready for when
     it builds.
   * Android and iOS devices have not been tested.
3. **`PdfWriter` file permissions.** Atomic saves create files with `0600`
   permissions, inherited from `tempfile`. This is pre-existing.
4. **Next model work:**
   * other scripts (Devanagari, CJK, Cyrillic) from the same pipeline;
   * table structure;
   * a dictionary-guided CTC beam search.

## Retraining

`tools/ocr_train/README.md` covers the whole pipeline, including NVIDIA GPUs
on Linux. The last run's workspace (venv, data, checkpoints, evaluation PDFs,
about 1 GB) is in `~/Library/Caches/flutter_pdf_core_ocr`, and can be deleted.

## Known limitations

* Printed Latin-script text only; no handwriting.
* Tables read column by column.
* No text inside photographs or white-on-black areas.
