"""Render a grid of training samples to eyeball the generator.

    python -I preview.py --fonts-dir FONTS --corpus-dir CORPUS --out grid.png
"""

import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import numpy as np  # noqa: E402
from PIL import Image  # noqa: E402

from render import LineRenderer, scan_faces  # noqa: E402
from textgen import TextSource  # noqa: E402


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fonts-dir", type=Path, required=True)
    parser.add_argument("--corpus-dir", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--count", type=int, default=24)
    parser.add_argument("--degrade", type=float, default=1.0)
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()

    started = time.time()
    faces = scan_faces(sorted(args.fonts_dir.rglob("*.tt[fc]")))
    text = TextSource(args.corpus_dir)
    print(f"{len(faces)} faces, {len(text.books)} books, setup {time.time() - started:.1f}s")
    renderer = LineRenderer(faces, degrade=args.degrade)
    rng = np.random.default_rng(args.seed)

    rows, labels, started = [], [], time.time()
    while len(rows) < args.count:
        label = text.sample(rng)
        image = renderer.render(label, rng, neighbour=lambda: text.sample(rng))
        if image is not None:
            rows.append(image)
            labels.append(label)
    elapsed = time.time() - started
    print(f"{args.count / elapsed:.0f} samples/s on one process")

    width = min(max(r.shape[1] for r in rows), 1200)
    grid = np.ones((len(rows) * 36, width), dtype=np.float32)
    for i, row in enumerate(rows):
        grid[i * 36 + 2:i * 36 + 34, :min(width, row.shape[1])] = 1.0 - row[:, :width]
    Image.fromarray((grid * 255).astype(np.uint8)).save(args.out)
    for i, label in enumerate(labels):
        print(f"{i:2d} {label!r}")


if __name__ == "__main__":
    main()
