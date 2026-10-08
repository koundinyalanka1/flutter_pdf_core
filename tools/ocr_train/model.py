"""The line recognizer: a small CNN feeding a bidirectional LSTM, trained with CTC.

`pdf_ocr::net` in Rust runs exactly this network (batch norm folded into the
convolutions by `export.py`). Changing the layers means changing both, and
the architecture name below, which the model file records.
"""

import torch
from torch import nn
from torch.nn.utils.rnn import pack_padded_sequence, pad_packed_sequence

ARCH = "cnn-bilstm"
HEIGHT = 32
CONVS = [(1, 16), (16, 32), (32, 64), (64, 64), (64, 96)]  # 3x3, padding 1
POOLS = [(2, 2), (2, 2), None, (2, 1), (2, 1)]  # max-pool after each conv, if any
HIDDEN = 128
STRIDE = 4  # input columns per output time step
# Blank paper the engine appends after every line before running the network,
# for models trained without packed sequences. Training batches pad short
# lines with paper (batches are sorted by width, so only a little), and the
# backward LSTM then runs through that padding before reaching the text.
# Packed sequences avoid it and are fast with CUDA, so CUDA training packs and
# its models need no trailing paper. They are three times slower on Apple's
# Metal backend, so Apple-GPU training pads and the engine matches. The model
# file records which applies (see export.py).
TRAILING_PAPER = 16


class Recognizer(nn.Module):
    def __init__(self, classes: int, packed: bool = False):
        super().__init__()
        self.packed = packed
        layers = []
        for (cin, cout), pool in zip(CONVS, POOLS):
            layers += [nn.Conv2d(cin, cout, 3, padding=1, bias=False), nn.BatchNorm2d(cout), nn.ReLU(inplace=True)]
            if pool:
                layers.append(nn.MaxPool2d(pool))
        self.features = nn.Sequential(*layers)
        features = CONVS[-1][1] * (HEIGHT // 16)
        self.rnn = nn.LSTM(features, HIDDEN, bidirectional=True)
        self.head = nn.Linear(2 * HIDDEN, classes)

    def forward(self, images: torch.Tensor, widths: torch.Tensor):
        """images [B, 1, 32, W] (W a multiple of 4); widths [B] on the CPU, each
        a multiple of 4. Returns logits [T, B, classes] and lengths [B]."""
        x = self.features(images)
        b, c, h, t = x.shape
        # Feature index c * h + row: channel-major, matching the Rust port.
        x = x.reshape(b, c * h, t).permute(2, 0, 1)
        lengths = (widths // STRIDE).clamp(min=1, max=t)
        if self.packed:
            x, _ = self.rnn(pack_padded_sequence(x, lengths, enforce_sorted=False))
            x, _ = pad_packed_sequence(x, total_length=t)
        else:
            x, _ = self.rnn(x)
        return self.head(x), lengths
