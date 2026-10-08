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

## Training on an NVIDIA GPU

Recommended: Linux with CUDA. On CUDA the LSTM uses packed sequences (exact
and fast with cuDNN), CTC loss runs on the GPU, and TF32 is enabled.

```bash
# Ubuntu with an NVIDIA driver. The two font packages are held-out
# evaluation fonts: none of them is in the training set.
sudo apt install git curl python3-venv fonts-dejavu-core fonts-urw-base35
DATA=~/ocr-data RUN=~/ocr-run
tools/ocr_train/prepare_data.sh "$DATA"
python3 -m venv ~/ocr-venv && ~/ocr-venv/bin/pip install torch numpy pillow fonttools

~/ocr-venv/bin/python -I tools/ocr_train/train.py \
  --fonts-dir "$DATA/google-fonts" --corpus-dir "$DATA/corpus" --out "$RUN" --steps 80000 \
  --eval-fonts /usr/share/fonts/truetype/dejavu/DejaVu{Sans,Serif,SansMono}.ttf \
               /usr/share/fonts/opentype/urw-base35/*.otf
```

* **CPU cores:** drawing training lines is CPU work done by `--workers`
  processes (default: all cores but two). On a fast GPU the data is the
  bottleneck, so use a machine with at least 16 vCPUs. Larger `--batch`
  sizes also help a big GPU.
* **Checkpoints:** written to `$RUN` at every evaluation (`last.pt`, and
  `best.pt` for the lowest held-out error). Add `--resume "$RUN/last.pt"` to
  continue an interrupted run, on the same kind of device.
* **Export:** works on any machine and needs only the CPU:

```bash
~/ocr-venv/bin/python -I tools/ocr_train/export.py --checkpoint "$RUN/best.pt" \
  --fonts-dir "$DATA/google-fonts" \
  --out rust/crates/pdf_ocr/models/latin.ocrm \
  --golden rust/crates/pdf_ocr/tests/fixtures
cd rust && cargo test -p pdf_ocr   # the golden tests hold Rust to PyTorch
```

On a Mac, the same commands train on the Apple GPU at about 650 lines a
second, a little over two hours for 80,000 steps of 64 lines. There, pass
macOS system fonts to `--eval-fonts`, for example
`/System/Library/Fonts/Supplemental/{Arial,Georgia,Verdana}.ttf`. The
bundled model is the step-32,000 checkpoint of such a run.

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

One detail looks odd but is deliberate. Models trained without packed
sequences (Apple GPU or CPU) see paper padding after short lines in every
batch, and their backward LSTM reads through that padding before it reaches
the text. The engine therefore appends `TRAILING_PAPER` columns of blank paper
to each line for such models. Packing would avoid the padding, but it trains
three times slower on Apple's Metal backend. CUDA runs pack, and their models
need no padding. `export.py` records which applies in the model file, and the
engine follows it.

## Measuring accuracy on real documents

```bash
swift tools/ocr_train/eval/make_eval_pdfs.swift /tmp/eval README.md docs/*.md CHANGELOG.md
cargo run --release -p pdf_cli -- ocr-eval /tmp/eval/Georgia.pdf      # renders, reads, scores
cargo run --release -p pdf_ocr --example ocr_debug -- in.pdf 1 out.png  # draws lines and words
```

`ocr-eval` works with any PDF that carries real text. It renders each page,
recognizes it, and compares the result with the page's own text.
