# `latin.ocrm`: the bundled recognizer

| | |
|---|---|
| Architecture | 5 × (3×3 conv + ReLU, batch norm folded) → bidirectional LSTM (128) → linear, CTC |
| Input | text line normalized to 32 px tall (`pdf_ocr::normalize`) |
| Characters | 208: ASCII, Latin-1, Windows-1252 punctuation (`tools/ocr_train/charset.py`) |
| Weights | 498,033, stored as float16 (997,744 bytes) |
| Training | `tools/ocr_train`, 32,000 of a planned 80,000 steps × 64 synthetic lines (checkpoint `best.pt`) |
| Training data | renders of ~330 OFL/Apache font faces; public-domain Gutenberg text; synthetic document strings |
| Held-out CER | 2.34% degraded / 0.99% clean synthetic lines; 1.45% on 72 real PDF pages in 24 unseen macOS fonts |

Known weaknesses:

* Underscores: the corpus cleaner stripped them, so `pdf_ops` often reads as
  `pdf ops`.
* `I`/`l`/`1` in fonts where the glyphs are identical.
* Typewriter faces.

To update the model, finish or resume training
(`train.py --resume RUN/last.pt`), then re-export with `export.py`. That
rewrites this file and `tests/fixtures/golden_*.bin`. Run
`cargo test -p pdf_ocr` afterwards.
