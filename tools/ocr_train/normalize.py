"""Line-image normalization, shared with the Rust engine (`pdf_ocr::normalize`).

The network only ever sees text through this function, so training and
inference must agree on it bit for bit in spirit and to ~1e-6 in practice.
Any change here has to be made identically in Rust; a golden test exported
by `export.py` compares the two.

1. Crop the line's ink box, padded by a fraction of its height.
2. Stretch contrast using the crop's own histogram: the median is paper
   (text lines are mostly background), the 99th percentile is ink.
3. Resample to HEIGHT rows: area-average when shrinking, bilinear when
   enlarging. Vertical pass first, then horizontal.

Output is float32 [HEIGHT, width] with ink near 1.0 and paper at 0.0.
"""

import math

import numpy as np

HEIGHT = 32
PAD_X = 0.3  # fraction of the ink height added left and right
PAD_Y = 0.2  # fraction of the ink height added above and below
MIN_CONTRAST = 32  # inverted grey levels between paper and ink, at least


def crop_rect(ink, image_w: int, image_h: int):
    """Ink box (x0, y0, x1, y1; floats, end-exclusive) -> integer crop, clamped."""
    x0, y0, x1, y1 = ink
    h = max(y1 - y0, 1.0)
    return (
        max(0, math.floor(x0 - PAD_X * h)),
        max(0, math.floor(y0 - PAD_Y * h)),
        min(image_w, math.ceil(x1 + PAD_X * h)),
        min(image_h, math.ceil(y1 + PAD_Y * h)),
    )


def _rank(cumulative, n: int, q: float) -> int:
    """Smallest value whose cumulative count exceeds floor(q * (n - 1))."""
    k = math.floor(q * (n - 1))
    return int(np.searchsorted(cumulative, k, side="right"))


def contrast_lut(region: np.ndarray) -> np.ndarray:
    inverted = 255 - region.astype(np.int64)
    cumulative = np.cumsum(np.bincount(inverted.ravel(), minlength=256))
    n = inverted.size
    lo = _rank(cumulative, n, 0.5)
    hi = max(_rank(cumulative, n, 0.99), lo + MIN_CONTRAST)
    values = (np.arange(256, dtype=np.float64)[::-1] - lo) / (hi - lo)  # index = grey level
    return np.clip(values, 0.0, 1.0).astype(np.float32)


def axis_taps(in_len: int, out_len: int):
    """Source indices and weights per output sample, as [out, taps] arrays.

    Shrinking: output i averages the input span [i*s, (i+1)*s), s = in/out,
    each source pixel weighted by its overlap / s. Enlarging: bilinear with
    pixel centres aligned, edges clamped. Taps are in increasing source order
    and padded with zero weights; the Rust port sums them in that order.
    """
    i = np.arange(out_len, dtype=np.float64)[:, None]
    if out_len < in_len:
        span = in_len / out_len
        start, end = i * span, (i + 1) * span
        j = np.floor(start) + np.arange(math.ceil(span) + 1)[None, :]
        overlap = np.minimum(end, j + 1.0) - np.maximum(start, j)
        valid = (j < np.minimum(np.ceil(end), in_len)) & (overlap > 0.0)
        weights = np.where(valid, overlap / span, 0.0)
        indices = np.where(valid, j, 0)
    else:
        source = (i + 0.5) * (in_len / out_len) - 0.5
        j0 = np.floor(source)
        t = source - j0
        indices = np.clip(np.concatenate([j0, j0 + 1], axis=1), 0, in_len - 1)
        weights = np.concatenate([1.0 - t, t], axis=1)
    return indices.astype(np.int64), weights.astype(np.float32)


def output_width(crop_w: int, crop_h: int) -> int:
    return max(4, math.floor(crop_w * HEIGHT / crop_h + 0.5))


def normalize(gray: np.ndarray, crop) -> np.ndarray:
    """gray: uint8 [H, W] page or canvas; crop: integer rect from crop_rect."""
    x0, y0, x1, y1 = crop
    region = gray[y0:y1, x0:x1]
    if region.size == 0:
        return np.zeros((HEIGHT, 4), dtype=np.float32)
    values = contrast_lut(region)[region]
    crop_h, crop_w = region.shape
    rows, row_weights = axis_taps(crop_h, HEIGHT)
    values = (values[rows] * row_weights[:, :, None]).sum(axis=1, dtype=np.float32)
    cols, col_weights = axis_taps(crop_w, output_width(crop_w, crop_h))
    return (values[:, cols] * col_weights[None]).sum(axis=2, dtype=np.float32)
