# Training the OCR recognizer

The OCR engine in `rust/crates/pdf_ocr` is plain Rust: no OCR or
machine-learning library runs inside the app. Its line recognizer is a small
neural network (a CNN feeding a bidirectional LSTM, decoded with CTC), and its
weights come from the tools in this directory. They run once, offline, on a
developer machine. Nothing here ships.

| File | Role |
|---|---|
| `charset.py` | The 208 characters the model reads (ASCII, Latin-1, Windows-1252 punctuation). Embedded in the model file. |
| `textgen.py` | Training text: public-domain prose, synthetic document strings (dates, amounts, IDs, addresses, table rows, form fields) and random characters. |
| `render.py` | Draws a line in a random font and degrades it like a scan or phone photo. |
| `normalize.py` | The line normalization the network sees. `pdf_ocr::normalize` is a port of it. |
| `model.py` | The network. `pdf_ocr::net` runs the same layers. |
| `train.py` | Training loop. Reports the character error rate (CER) on held-out fonts. |
| `export.py` | Folds batch norm, writes the `.ocrm` model file and golden test data. |
| `preview.py` | Saves a grid of training samples so the generator can be checked by eye. |
| `prepare_data.sh` | Downloads the fonts and the corpus. |
| `eval/make_eval_pdfs.swift` | Builds evaluation PDFs in fonts the model never saw (macOS). |

## Reproducing the bundled model

```bash
DATA=/tmp/ocr-data
tools/ocr_train/prepare_data.sh "$DATA"
python3 -m venv /tmp/ocr-venv && /tmp/ocr-venv/bin/pip install torch numpy pillow fonttools

# Held-out evaluation fonts: macOS system fonts, none of them in the training set.
/tmp/ocr-venv/bin/python -I tools/ocr_train/train.py \
  --fonts-dir "$DATA/google-fonts" --corpus-dir "$DATA/corpus" --out /tmp/ocr-run \
  --steps 80000 --eval-fonts /System/Library/Fonts/Supplemental/{Arial,Georgia,Verdana}.ttf

/tmp/ocr-venv/bin/python -I tools/ocr_train/export.py --checkpoint /tmp/ocr-run/best.pt \
  --fonts-dir "$DATA/google-fonts" \
  --out rust/crates/pdf_ocr/models/latin.ocrm \
  --golden rust/crates/pdf_ocr/tests/fixtures
cd rust && cargo test -p pdf_ocr   # the golden tests hold Rust to PyTorch
```

On an Apple M4 Pro (Metal backend), training runs at about 650 lines a second,
so the full 80,000 steps of 64 lines take a little over two hours. The bundled
model is the step-32,000 checkpoint of that run, interrupted at 40%. Finishing
the run (`--resume RUN/last.pt`) should lower its error rate further.

## Data and licences

* **Fonts:** about 330 faces from 64 families in
  [google/fonts](https://github.com/google/fonts), all under the SIL Open Font
  License or Apache 2.0. Fonts are rendered to images during training. No
  font data is in the weights or in this repository.
* **Text:** 39 public-domain books from Project Gutenberg (English, French,
  German, Spanish, Italian and Portuguese), plus synthetic strings generated
  by `textgen.py`.
* **Evaluation:** system fonts on the training machine, used only to measure
  accuracy, never to train.

## Changing the network or the normalization

`model.py` and `normalize.py` have Rust twins (`pdf_ocr::net` and
`pdf_ocr::normalize`). Change both together, re-export, and run
`cargo test -p pdf_ocr`. `tests/golden.rs` compares the two implementations
on the files `export.py` writes. The model file records its architecture, and
the engine rejects any file whose shapes do not match it.

Two details look odd but are deliberate:

* The engine appends `TRAILING_PAPER` columns of blank paper after each line.
  Training batches pad short lines with paper, and the backward LSTM reads
  through that padding before it reaches the text. Packing sequences would
  avoid the padding, but it trains three times slower on the Metal backend.
* CTC loss is computed on the CPU, because the Metal backend does not
  implement it.

## Measuring accuracy on real documents

```bash
swift tools/ocr_train/eval/make_eval_pdfs.swift /tmp/eval README.md docs/*.md CHANGELOG.md
cargo run --release -p pdf_cli -- ocr-eval /tmp/eval/Georgia.pdf      # renders, reads, scores
cargo run --release -p pdf_ocr --example ocr_debug -- in.pdf 1 out.png  # draws lines and words
```

`ocr-eval` works with any PDF that carries real text. It renders each page,
recognizes it, and compares the result with the page's own text.
