"""Export a checkpoint as the engine's model file, plus golden test data.

    python -I export.py --checkpoint RUN/best.pt --fonts-dir FONTS \
        --out ../../rust/crates/pdf_ocr/models/latin.ocrm \
        --golden ../../rust/crates/pdf_ocr/tests/fixtures

Model file layout (all integers little-endian):

    b"PDFOCRM1" | u32 header length | UTF-8 JSON header | zeros to a 16-byte
    boundary | tensor data: float16, each tensor at a 16-byte aligned offset
    relative to the start of the data

Batch norm is folded into the convolutions and the LSTM's two bias vectors
are summed, so the engine runs plain conv + ReLU + max-pool + LSTM + linear.

Golden files let the Rust tests prove they compute what PyTorch does:
`golden_net.bin` (a rendered line, network input and reference logits from
the float16 weights) and `golden_normalize.bin` (normalization cases).
"""

import argparse
import json
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import numpy as np  # noqa: E402
import torch  # noqa: E402
import torch.nn.functional as F  # noqa: E402
from torch import nn  # noqa: E402

from charset import CHARSET, CLASSES, decode_greedy  # noqa: E402
from model import ARCH, CONVS, HEIGHT, HIDDEN, POOLS, STRIDE, TRAILING_PAPER, Recognizer  # noqa: E402
from normalize import crop_rect, normalize  # noqa: E402

MAGIC = b"PDFOCRM1"


def folded(model: Recognizer):
    """(name, float64 array) in file order."""
    tensors = []
    convs = [m for m in model.features if isinstance(m, nn.Conv2d)]
    norms = [m for m in model.features if isinstance(m, nn.BatchNorm2d)]
    for i, (conv, norm) in enumerate(zip(convs, norms)):
        scale = norm.weight / torch.sqrt(norm.running_var + norm.eps)
        tensors.append((f"conv{i}.weight", conv.weight * scale[:, None, None, None]))
        tensors.append((f"conv{i}.bias", norm.bias - norm.running_mean * scale))
    for direction, suffix in (("forward", ""), ("backward", "_reverse")):
        rnn = model.rnn
        tensors.append((f"lstm.{direction}.w_ih", getattr(rnn, f"weight_ih_l0{suffix}")))
        tensors.append((f"lstm.{direction}.w_hh", getattr(rnn, f"weight_hh_l0{suffix}")))
        tensors.append((f"lstm.{direction}.bias",
                        getattr(rnn, f"bias_ih_l0{suffix}") + getattr(rnn, f"bias_hh_l0{suffix}")))
    tensors.append(("head.weight", model.head.weight))
    tensors.append(("head.bias", model.head.bias))
    return [(name, t.detach().cpu().double().numpy()) for name, t in tensors]


def write_model(path: Path, tensors, info: dict, trailing_paper: int):
    entries, blob = [], bytearray()
    for name, array in tensors:
        data = array.astype("<f2").tobytes()
        entries.append({"name": name, "shape": list(array.shape), "offset": len(blob)})
        blob += data
        blob += bytes(-len(blob) % 16)
    header = {
        "format": 1, "arch": ARCH, "height": HEIGHT, "stride": STRIDE,
        "trailing_paper": trailing_paper, "charset": CHARSET,
        "convs": CONVS, "pools": POOLS, "hidden": HIDDEN, "tensors": entries, **info,
    }
    encoded = json.dumps(header, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    prefix = MAGIC + struct.pack("<I", len(encoded)) + encoded
    prefix += bytes(-len(prefix) % 16)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(prefix + bytes(blob))
    return len(prefix) + len(blob)


def reference_logits(weights: dict, image: np.ndarray) -> np.ndarray:
    """The network in plain tensor operations, float32 -- the Rust port's spec."""
    x = torch.from_numpy(image)[None, None]
    for i, pool in enumerate(POOLS):
        x = F.relu(F.conv2d(x, weights[f"conv{i}.weight"], weights[f"conv{i}.bias"], padding=1))
        if pool:
            x = F.max_pool2d(x, pool)
    _, c, h, t = x.shape
    sequence = x[0].reshape(c * h, t).T  # [T, features], feature = channel * h + row
    outputs = []
    for direction in ("forward", "backward"):
        w_ih, w_hh, bias = (weights[f"lstm.{direction}.{k}"] for k in ("w_ih", "w_hh", "bias"))
        state, cell = torch.zeros(HIDDEN), torch.zeros(HIDDEN)
        out = torch.zeros(t, HIDDEN)
        steps = range(t) if direction == "forward" else range(t - 1, -1, -1)
        for step in steps:
            gates = w_ih @ sequence[step] + bias + w_hh @ state
            i, f, g, o = gates.split(HIDDEN)  # PyTorch gate order
            cell = torch.sigmoid(f) * cell + torch.sigmoid(i) * torch.tanh(g)
            state = torch.sigmoid(o) * torch.tanh(cell)
            out[step] = state
        outputs.append(out)
    features = torch.cat(outputs, dim=1)
    return (features @ weights["head.weight"].T + weights["head.bias"]).numpy()


def network_input(line: np.ndarray, trailing_paper: int) -> np.ndarray:
    """The normalized line as the engine feeds it: width rounded up to the
    stride, then trailing paper."""
    width = -(-line.shape[1] // STRIDE) * STRIDE + trailing_paper
    padded = np.zeros((HEIGHT, width), dtype=np.float32)
    padded[:, :line.shape[1]] = line
    return padded


def write_golden(directory: Path, tensors, fonts_dir: Path, trailing_paper: int):
    from PIL import Image, ImageDraw, ImageFont

    directory.mkdir(parents=True, exist_ok=True)
    weights = {name: torch.from_numpy(a.astype(np.float16).astype(np.float32)) for name, a in tensors}

    font_path = next(iter(sorted(fonts_dir.rglob("Tinos-Regular.ttf"))), None) or next(iter(sorted(fonts_dir.rglob("*.ttf"))))
    page = Image.new("L", (900, 120), 247)
    ImageDraw.Draw(page).text((30, 70), "Golden 42: café, 3.50 €!", font=ImageFont.truetype(str(font_path), 40),
                              fill=18, anchor="ls")
    pixels = np.asarray(page)
    ys, xs = np.nonzero(pixels < 128)
    ink = (float(xs.min()), float(ys.min()), float(xs.max() + 1), float(ys.max() + 1))
    line = normalize(pixels, crop_rect(ink, pixels.shape[1], pixels.shape[0]))
    image = network_input(line, trailing_paper)
    logits = reference_logits(weights, image)
    text = decode_greedy(logits[: -(-line.shape[1] // STRIDE)].argmax(1))
    with (directory / "golden_net.bin").open("wb") as out:
        out.write(b"PDFOCRG1" + struct.pack("<II", *image.shape) + image.astype("<f4").tobytes())
        out.write(struct.pack("<II", *logits.shape) + logits.astype("<f4").tobytes())
        encoded = text.encode("utf-8")
        out.write(struct.pack("<I", len(encoded)) + encoded)

    rng = np.random.default_rng(7)
    cases = []
    # Shrinking (tall crop), enlarging (short crop) and a box clamped at the edges.
    for size, ink_box in ((40, None), (11, None), (24, (-3.5, -2.25, 400.0, 61.0))):
        canvas = Image.new("L", (420, 60), int(rng.integers(190, 250)))
        ImageDraw.Draw(canvas).text((12, 44), "Normalize: Ågå 0.5%", font=ImageFont.truetype(str(font_path), size),
                                    fill=int(rng.integers(0, 60)), anchor="ls")
        noisy = np.clip(np.asarray(canvas, dtype=np.float32) + rng.normal(0, 6, (60, 420)), 0, 255).astype(np.uint8)
        if ink_box is None:
            ys, xs = np.nonzero(noisy < 110)
            ink_box = (float(xs.min()) + 0.25, float(ys.min()) - 0.5, float(xs.max()) + 1.75, float(ys.max()) + 0.5)
        crop = crop_rect(ink_box, noisy.shape[1], noisy.shape[0])
        cases.append((noisy, ink_box, crop, normalize(noisy, crop)))
    with (directory / "golden_normalize.bin").open("wb") as out:
        out.write(b"PDFOCRN1" + struct.pack("<I", len(cases)))
        for pixels, ink_box, crop, result in cases:
            out.write(struct.pack("<II", pixels.shape[1], pixels.shape[0]) + pixels.tobytes())
            out.write(struct.pack("<4d", *ink_box) + struct.pack("<4I", *crop))
            out.write(struct.pack("<I", result.shape[1]) + result.astype("<f4").tobytes())
    return text


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkpoint", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--golden", type=Path)
    parser.add_argument("--fonts-dir", type=Path)
    parser.add_argument("--name", default="latin-print")
    args = parser.parse_args()

    state = torch.load(args.checkpoint, map_location="cpu")
    model = Recognizer(CLASSES)
    model.load_state_dict(state["model"])
    model.eval()
    tensors = folded(model)
    info = {"name": args.name, "steps": state.get("step"),
            "cer": {k: round(float(v), 5) for k, v in state.get("scores", {}).items()}}
    # Models trained with packed sequences (CUDA) never saw padding.
    trailing_paper = 0 if state.get("packed", False) else TRAILING_PAPER
    size = write_model(args.out, tensors, info, trailing_paper)
    print(f"wrote {args.out} ({size:,} bytes, {sum(a.size for _, a in tensors):,} weights)")
    if args.golden:
        text = write_golden(args.golden, tensors, args.fonts_dir, trailing_paper)
        print(f"golden line decodes as {text!r}")


if __name__ == "__main__":
    main()
