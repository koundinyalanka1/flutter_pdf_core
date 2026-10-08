# OCR: progress and next steps

Status on 2026-10-08, evening. The first part of the work is committed
("ocr progress"). Changes since then are not committed yet.

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

* **Accuracy:** the bundled step-32,000 model scores 1.08% CER and 7.6% WER
  on 72 pages typeset in 24 macOS fonts it never saw, at 300 dpi.
* **Earlier figure:** this was first reported as 1.45%. About a quarter of
  those errors were in the reference text, from the MacRoman ligature bug, now
  fixed.
* **Speed:** about 0.1–0.3 s per page on an Apple M4 Pro using all cores, and
  about 1.7 s on one core.
* **Size:** the Android arm64 `libpdf_ffi.so` grows from 2.83 MB to 4.23 MB
  (stripped), of which about 1 MB is the model.
* **Tests:** `cargo test` passes for `pdf_ocr` (17 unit, 2 golden, 1
  orientation, 4 end-to-end) and for `pdf_ffi`, `pdf_text` and `pdf_ai`.
  `flutter test` passes, and `flutter analyze` reports no new issues.

## Not done yet, in order

1. **Finish training (in progress).**
   * The first run was lost when `/tmp` was cleared, so a full 80,000-step
     retrain is running.
   * The new run adds code-style tokens to the training text: snake_case,
     CamelCase, paths and slash-joined words. These target the underscore,
     `I`/`l` and `/` errors.
   * Everything lives in `~/Library/Caches/flutter_pdf_core_ocr`, which
     survives restarts. The log is `runs/v2.log`; checkpoints are written to
     `runs/v2/` every 4,000 steps.
   * If the run stops, resume it with `./train_v2.sh --resume runs/v2/last.pt`.
   * When it finishes:
     * run `export.py --checkpoint runs/v2/best.pt --golden …`;
     * run `cargo test -p pdf_ocr`;
     * score it with `eval/run_eval.sh eval/pdfs 300`. The baseline to beat is
       `eval/baseline_32k_fixed_truth.txt`: 1.08% CER.
2. **Rebuild the checked-in native binaries** (`scripts/build_android.sh`,
   `build_ios.sh`, `build_macos.sh`). They do not contain OCR yet, so the
   Dart calls throw `OCR_UNAVAILABLE`. Cross-compiling for Android arm64 and
   iOS has been checked and works.
3. **Run clippy.** It is not installed locally; CI runs it.
4. **Run the on-device integration tests.** An OCR test is now in
   `example/integration_test`, with the scan embedded as base64.
   * It can't run yet: the example's macOS build fails before any test starts.
   * The committed Xcode project uses Swift Package Manager, and the plugin's
     `macos/flutter_pdf_core/Package.swift` depends on `../FlutterFramework`,
     which does not exist. That is a pre-existing setup issue, not an OCR one.
   * The macOS dylib has been rebuilt with OCR (step-32k model).
5. **Model improvements.**
   * The running retrain covers underscores, `I`/`l` and `/`, through the new
     code-style tokens.
   * Still open: more typewriter faces in the training fonts.

## Bugs found along the way (pre-existing)

* ~~`pdf_text/src/font.rs`: the MacRoman table lacked 0xDE→ﬁ and 0xDF→ﬂ.~~
  Fixed: the table now covers the full range (127 codes).
* `PdfWriter` atomic saves create files with `0600` permissions (inherited
  from `tempfile`).

## Known limitations

* Printed Latin-script text only; no handwriting.
* Tables read column by column.
* No text inside photographs or white-on-black areas.
