"""The recognizer's character set.

Index 0 is the CTC blank; character ``CHARSET[i]`` is class ``i + 1``. The
exported model file carries this string, so the Rust engine never keeps a
copy of its own that could drift from what the network was trained on.
"""

import unicodedata

ASCII = "".join(chr(c) for c in range(0x20, 0x7F))
LATIN1_SYMBOLS = "¡¢£¥§©ª«¬®°±²³µ¶·¹º»¼½¾¿"
LATIN1_LETTERS = "".join(chr(c) for c in range(0xC0, 0x100))  # includes × and ÷
# Windows-1252 additions: what Western documents actually type beyond Latin-1.
CP1252_EXTRAS = "€‚ƒ„…†‡‰Š‹ŒŽ‘’“”•–—™š›œžŸ"

CHARSET = ASCII + LATIN1_SYMBOLS + LATIN1_LETTERS + CP1252_EXTRAS
assert len(set(CHARSET)) == len(CHARSET), "duplicate characters in charset"

BLANK = 0
CLASSES = len(CHARSET) + 1
INDEX = {ch: i + 1 for i, ch in enumerate(CHARSET)}

# Look-alikes folded onto charset members before text is used as a label.
FOLD = {
    " ": " ", " ": " ", " ": " ", " ": " ", " ": " ",
    "\t": " ", "\r": " ", "\n": " ",
    "­": "",  # soft hyphen: invisible unless at a line break
    "‐": "-", "‑": "-", "‒": "–", "―": "—", "−": "-",
    "′": "'", "″": '"', "ʼ": "’", "ʻ": "‘",
    "ﬀ": "ff", "ﬁ": "fi", "ﬂ": "fl", "ﬃ": "ffi", "ﬄ": "ffl",
    "․": ".", "‧": "·",
}


def fold(text: str) -> str:
    """NFC-normalize, fold look-alikes and collapse runs of spaces."""
    text = unicodedata.normalize("NFC", text)
    text = "".join(FOLD.get(ch, ch) for ch in text)
    return " ".join(text.split())


def encodable(text: str) -> bool:
    return all(ch in INDEX for ch in text)


def encode(text: str) -> list[int]:
    return [INDEX[ch] for ch in text]


def decode_greedy(indices) -> str:
    """Collapse repeats, then drop blanks (best-path CTC decoding)."""
    out, previous = [], BLANK
    for index in indices:
        index = int(index)
        if index != previous and index != BLANK:
            out.append(CHARSET[index - 1])
        previous = index
    return "".join(out)
