# `latin.ocrm`: the bundled recognizer

| | |
|---|---|
| Architecture | 5 × (3×3 conv + ReLU, batch norm folded) → bidirectional LSTM (128) → linear, CTC |
| Input | text line normalized to 32 px tall (`pdf_ocr::normalize`) |
| Characters | 208: ASCII, Latin-1, Windows-1252 punctuation (`tools/ocr_train/charset.py`) |
| Weights | 498,033, stored as float16 (997,744 bytes) |
| Training | `tools/ocr_train`, 80,000 steps × 64 synthetic lines on an Apple M4 Pro (about 2 hours); this is the best checkpoint, step 76,000 |
| Training data | renders of ~330 OFL/Apache font faces; public-domain Gutenberg text; synthetic document strings and code-style identifiers |
| Held-out CER | 1.22% degraded / 0.51% clean synthetic lines; 0.30% (WER 1.8%) on 72 real PDF pages in 24 unseen macOS fonts |

Weakest fonts on the benchmark: Big Caslon 0.85%, Hoefler Text 0.72%,
Courier New 0.68% and Menlo 0.61% (character error rate). The remaining
errors are mostly `I`/`l`/`1` in fonts where those glyphs look alike, and
dashes of different lengths.

To update the model, train (`train.py`, or `--resume RUN/last.pt` to continue
a run), then re-export with `export.py`. That
rewrites this file and `tests/fixtures/golden_*.bin`. Run
`cargo test -p pdf_ocr` afterwards.
