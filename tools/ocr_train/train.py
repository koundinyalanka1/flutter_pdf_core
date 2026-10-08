"""Train the line recognizer on synthetic lines.

    python -I train.py --fonts-dir FONTS --corpus-dir CORPUS \
        --eval-fonts /System/Library/Fonts/Supplemental/Arial.ttf ... --out RUN

Fonts used for evaluation should not be among the training fonts: the
character error rate it reports is then a fair estimate for unseen fonts.
"""

import argparse
import json
import math
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import numpy as np  # noqa: E402
import torch  # noqa: E402
from torch.utils.data import DataLoader, IterableDataset, get_worker_info  # noqa: E402

from charset import CLASSES, decode_greedy, encode  # noqa: E402
from model import STRIDE, Recognizer  # noqa: E402
from render import LineRenderer, scan_faces  # noqa: E402
from textgen import TextSource  # noqa: E402


def steps_needed(label: str) -> int:
    """CTC needs a time step per character, plus a blank between repeats."""
    return len(label) + sum(a == b for a, b in zip(label, label[1:]))


def collate(samples):
    widths = [math.ceil(image.shape[1] / STRIDE) * STRIDE for image, _ in samples]
    batch = np.zeros((len(samples), 1, 32, max(widths)), dtype=np.float32)
    for i, (image, _) in enumerate(samples):
        batch[i, 0, :, :image.shape[1]] = image
    labels = [label for _, label in samples]
    targets = [index for label in labels for index in encode(label)]
    return (torch.from_numpy(batch), torch.tensor(widths), torch.tensor(targets, dtype=torch.long),
            torch.tensor([len(label) for label in labels], dtype=torch.long), labels)


class Lines(IterableDataset):
    """Endless batches of similar-width lines (sorted pools waste less padding)."""

    def __init__(self, faces, corpus_dir, batch, seed, degrade=1.0, pool=8):
        self.faces, self.corpus_dir, self.batch = faces, corpus_dir, batch
        self.seed, self.degrade, self.pool = seed, degrade, pool

    def __iter__(self):
        info = get_worker_info()
        rng = np.random.default_rng([self.seed, info.id if info else 0])
        text = TextSource(self.corpus_dir)
        renderer = LineRenderer(self.faces, self.degrade)
        while True:
            samples = []
            while len(samples) < self.batch * self.pool:
                label = text.sample(rng)
                image = renderer.render(label, rng, neighbour=lambda: text.sample(rng))
                if image is not None and image.shape[1] // STRIDE >= steps_needed(label):
                    samples.append((image, label))
            samples.sort(key=lambda s: s[0].shape[1])
            batches = [samples[i:i + self.batch] for i in range(0, len(samples), self.batch)]
            for index in rng.permutation(len(batches)):
                yield collate(batches[index])


def edit_distance(a: str, b: str) -> int:
    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        current = [i]
        for j, cb in enumerate(b, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (ca != cb)))
        previous = current
    return previous[-1]


def fixed_set(faces, corpus_dir, count, degrade, seed):
    rng = np.random.default_rng(seed)
    text = TextSource(corpus_dir)
    renderer = LineRenderer(faces, degrade)
    samples = []
    while len(samples) < count:
        label = text.sample(rng)
        image = renderer.render(label, rng, neighbour=lambda: text.sample(rng))
        if image is not None and image.shape[1] // STRIDE >= steps_needed(label):
            samples.append((image, label))
    samples.sort(key=lambda s: s[0].shape[1])
    return [collate(samples[i:i + 64]) for i in range(0, len(samples), 64)]


@torch.no_grad()
def evaluate(model, batches, device):
    model.eval()
    errors = chars = 0
    examples = []
    for images, widths, _, _, labels in batches:
        logits, lengths = model(images.to(device), widths)
        best = logits.argmax(2).cpu().numpy()
        for i, label in enumerate(labels):
            predicted = decode_greedy(best[:lengths[i], i])
            errors += edit_distance(predicted, label)
            chars += len(label)
            if predicted != label and len(examples) < 6:
                examples.append((label, predicted))
    model.train()
    return errors / max(chars, 1), examples


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fonts-dir", type=Path, required=True)
    parser.add_argument("--corpus-dir", type=Path, required=True)
    parser.add_argument("--eval-fonts", type=Path, nargs="+", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--steps", type=int, default=60_000)
    parser.add_argument("--batch", type=int, default=64)
    parser.add_argument("--lr", type=float, default=2e-3)
    parser.add_argument("--workers", type=int, default=max(1, (os.cpu_count() or 4) - 2),
                        help="data processes; drawing lines is CPU work, so give it most cores")
    parser.add_argument("--eval-every", type=int, default=2000)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--resume", type=Path)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    if torch.cuda.is_available():
        device = torch.device("cuda")
        # TF32 matmuls and convolutions: much faster on Ampere and later, and
        # far more precise than this model needs.
        torch.backends.cuda.matmul.allow_tf32 = True
        torch.backends.cudnn.allow_tf32 = True
    elif torch.backends.mps.is_available():
        device = torch.device("mps")
    else:
        device = torch.device("cpu")
    torch.manual_seed(args.seed)
    faces = scan_faces(sorted(args.fonts_dir.rglob("*.tt[fc]")))
    eval_faces = scan_faces(args.eval_fonts)
    print(f"device {device}; {len(faces)} training faces; {len(eval_faces)} held-out faces", flush=True)
    eval_sets = {
        "degraded": fixed_set(eval_faces, args.corpus_dir, 1500, 0.6, 1234),
        "clean": fixed_set(eval_faces, args.corpus_dir, 500, 0.0, 4321),
    }

    # Packed sequences are exact and fast with cuDNN; elsewhere the model pads
    # (see TRAILING_PAPER in model.py).
    model = Recognizer(CLASSES, packed=device.type == "cuda").to(device)
    optimizer = torch.optim.AdamW(model.parameters(), lr=args.lr, weight_decay=1e-4)
    schedule = torch.optim.lr_scheduler.OneCycleLR(optimizer, max_lr=args.lr, total_steps=args.steps,
                                                   pct_start=0.04, div_factor=20, final_div_factor=200)
    step, best = 0, float("inf")
    if args.resume:
        state = torch.load(args.resume, map_location="cpu")
        model.load_state_dict(state["model"])
        optimizer.load_state_dict(state["optimizer"])
        schedule.load_state_dict(state["schedule"])
        step, best = state["step"], state.get("best", best)
        if state.get("packed", False) != model.packed:
            raise SystemExit("resume on the same kind of device the run started on")
    print(f"{sum(p.numel() for p in model.parameters()):,} parameters", flush=True)

    ctc = torch.nn.CTCLoss(blank=0, zero_infinity=True)
    loader = DataLoader(Lines(faces, args.corpus_dir, args.batch, args.seed + step),
                        batch_size=None, num_workers=args.workers, prefetch_factor=4,
                        persistent_workers=True, pin_memory=device.type == "cuda")
    log = (args.out / "log.jsonl").open("a")
    started, seen, running = time.time(), 0, []
    model.train()
    for images, widths, targets, target_lengths, labels in loader:
        if step >= args.steps:
            break
        logits, lengths = model(images.to(device, non_blocking=True), widths)
        loss = ctc(logits.log_softmax(2), targets.to(device), lengths.to(device), target_lengths.to(device))
        optimizer.zero_grad(set_to_none=True)
        loss.backward()
        torch.nn.utils.clip_grad_norm_(model.parameters(), 5.0)
        optimizer.step()
        schedule.step()
        step += 1
        seen += len(labels)
        running.append(loss.item())
        if step % 100 == 0:
            rate = seen / (time.time() - started)
            print(f"step {step} loss {np.mean(running):.4f} lr {schedule.get_last_lr()[0]:.2e} "
                  f"{rate:.0f} lines/s", flush=True)
            log.write(json.dumps({"step": step, "loss": float(np.mean(running))}) + "\n")
            running = []
        if step % args.eval_every == 0 or step == args.steps:
            scores = {}
            for name, batches in eval_sets.items():
                scores[name], examples = evaluate(model, batches, device)
            print(f"eval step {step}: " + " ".join(f"{k} CER {v:.2%}" for k, v in scores.items()), flush=True)
            for label, predicted in examples:
                print(f"    {label!r}\n -> {predicted!r}", flush=True)
            log.write(json.dumps({"step": step, **{f"cer_{k}": v for k, v in scores.items()}}) + "\n")
            log.flush()
            state = {"model": model.state_dict(), "optimizer": optimizer.state_dict(),
                     "schedule": schedule.state_dict(), "step": step, "best": best, "scores": scores,
                     "packed": model.packed}
            torch.save(state, args.out / "last.pt")
            if scores["degraded"] < best:
                best = state["best"] = scores["degraded"]
                torch.save(state, args.out / "best.pt")


if __name__ == "__main__":
    main()
