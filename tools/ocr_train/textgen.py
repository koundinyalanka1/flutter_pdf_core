"""Training text: what a printed line of a real document might say.

Three sources, mixed per sample:

* spans of public-domain prose (Project Gutenberg, several languages), so the
  recognizer's sequence model learns ordinary words and accents;
* synthetic document strings (dates, amounts, identifiers, addresses, table
  rows, form fields), which prose almost never contains;
* random characters, so every symbol in the charset is seen and the model
  cannot lean on spelling alone.
"""

import re
import string
from pathlib import Path

import numpy as np

from charset import CHARSET, encodable, fold

LETTERS = string.ascii_letters
ACCENTED = "àáâäãåæçèéêëìíîïñòóôöõøœùúûüýÿßÀÁÂÄÃÅÆÇÈÉÊËÌÍÎÏÑÒÓÔÖÕØŒÙÚÛÜÝŸ"
MONTHS = ["January", "February", "March", "April", "May", "June", "July",
          "August", "September", "October", "November", "December"]
WEEKDAYS = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
STREETS = ["Main St", "Oak Avenue", "Baker Street", "Rue de Rivoli", "Hauptstraße",
           "Calle Mayor", "Elm Rd", "Park Lane", "Station Road", "Via Roma", "King's Road"]
CITIES = ["London", "Paris", "Berlin", "Madrid", "Mountain View, CA", "New York, NY",
          "Zürich", "Montréal", "São Paulo", "Köln", "Lisboa", "Austin, TX"]
TLDS = ["com", "org", "net", "io", "co.uk", "de", "fr", "edu", "gov"]
LABELS = ["Name", "Date", "Address", "Invoice No.", "Total", "Subtotal", "Tax", "Phone",
          "Email", "Account", "Reference", "Due date", "Amount due", "Signature", "Qty",
          "Description", "Unit price", "Customer ID", "Order #", "Policy number"]


def _strip_gutenberg(text: str) -> str:
    start = re.search(r"\*\*\* ?START OF (THE|THIS) PROJECT GUTENBERG[^\n]*\n", text)
    end = re.search(r"\*\*\* ?END OF (THE|THIS) PROJECT GUTENBERG", text)
    if start:
        text = text[start.end():end.start() if end else None]
    # Gutenberg marks italics with underscores and em dashes with "--".
    text = text.replace("_", "")
    return text


class TextSource:
    def __init__(self, corpus_dir: Path):
        self.books: list[str] = []
        self.weights: list[float] = []
        for path in sorted(Path(corpus_dir).glob("pg*.txt")):
            raw = path.read_text(encoding="utf-8", errors="ignore")
            language = re.search(r"^Language: *(.+)$", raw, re.M)
            body = fold(_strip_gutenberg(raw))
            # Keep only characters the model can learn; other scripts become gaps.
            body = "".join(ch if ch in CHARSET else " " for ch in body)
            body = " ".join(body.split())
            if len(body) < 10_000:
                continue
            english = language is not None and language.group(1).strip() == "English"
            self.books.append(body)
            # Other languages are fewer books; weight them up for their accents.
            self.weights.append(1.0 if english else 2.5)
        if not self.books:
            raise SystemExit(f"no usable pg*.txt files in {corpus_dir}")
        total = sum(self.weights)
        self.weights = [w / total for w in self.weights]
        self.vocabulary = sorted({w for book in self.books[:8] for w in book.split(" ")
                                  if 2 <= len(w) <= 14 and w.isalpha()})

    # -- sources ---------------------------------------------------------------

    def prose(self, rng, max_chars: int) -> str:
        book = self.books[rng.choice(len(self.books), p=self.weights)]
        start = int(rng.integers(0, len(book) - max_chars - 1))
        start = book.find(" ", start) + 1
        words = int(rng.choice([1, 2, 3, 4, 6, 8, 10, 12, 16], p=[.06, .06, .08, .1, .14, .16, .16, .14, .1]))
        piece = " ".join(book[start:start + max_chars * 2].split(" ")[:words])[:max_chars].strip()
        piece = piece.replace("--", "—" if rng.random() < 0.6 else " - ")
        if rng.random() < 0.5:
            piece = _curly_quotes(piece)
        case = rng.random()
        if case < 0.04:
            piece = piece.upper()
        elif case < 0.07:
            piece = piece.title()
        return piece

    def document(self, rng) -> str:
        return DOCUMENT_TEMPLATES[int(rng.integers(len(DOCUMENT_TEMPLATES)))](self, rng)

    def random_chars(self, rng) -> str:
        length = int(rng.integers(2, 24))
        chars = rng.choice(list(CHARSET), size=length)
        text = "".join(chars)
        return " ".join(text.split())

    def word_salad(self, rng) -> str:
        words = [self.vocabulary[int(rng.integers(len(self.vocabulary)))]
                 for _ in range(int(rng.integers(1, 9)))]
        out = []
        for word in words:
            if rng.random() < 0.25:  # sprinkle accents a prose sample rarely has
                i = int(rng.integers(len(word)))
                word = word[:i] + ACCENTED[int(rng.integers(len(ACCENTED)))] + word[i + 1:]
            if rng.random() < 0.15:
                word = word.capitalize()
            out.append(word)
        return " ".join(out)

    def sample(self, rng, max_chars: int = 72) -> str:
        """One line of text, guaranteed encodable and non-empty."""
        for _ in range(20):
            r = rng.random()
            if r < 0.55:
                text = self.prose(rng, max_chars)
            elif r < 0.80:
                text = self.document(rng)
            elif r < 0.90:
                text = self.word_salad(rng)
            else:
                text = self.random_chars(rng)
            text = fold(text)[:max_chars].strip()
            if text and encodable(text):
                return text
        return "OCR"


def _curly_quotes(text: str) -> str:
    text = re.sub(r'(^|[\s(\[])"', r"\1“", text)
    text = text.replace('"', "”")
    text = re.sub(r"(^|[\s(\[])'", r"\1‘", text)
    return text.replace("'", "’")


# -- synthetic document strings ------------------------------------------------

def _digits(rng, n):
    return "".join(str(int(d)) for d in rng.integers(0, 10, size=n))


def _amount(rng):
    value = float(rng.choice([rng.uniform(0, 100), rng.uniform(0, 10_000), rng.uniform(0, 2e6)]))
    style = rng.random()
    if style < 0.5:
        text = f"{value:,.2f}"
        return rng.choice(["$", "£", "€", "", "USD ", "$ "]) + text
    if style < 0.8:  # continental style: 1.234,56 €
        text = f"{value:,.2f}".replace(",", " ").replace(".", ",").replace(" ", ".")
        return text + rng.choice([" €", "€", " EUR", ""])
    return f"{value:.{int(rng.integers(0, 4))}f}"


def _date(rng):
    y, m, d = int(rng.integers(1900, 2040)), int(rng.integers(1, 13)), int(rng.integers(1, 29))
    return str(rng.choice([
        f"{m:02d}/{d:02d}/{y}", f"{d:02d}.{m:02d}.{y}", f"{y}-{m:02d}-{d:02d}",
        f"{MONTHS[m - 1]} {d}, {y}", f"{d} {MONTHS[m - 1][:3]} {y}",
        f"{WEEKDAYS[int(rng.integers(7))]}, {d} {MONTHS[m - 1]} {y}",
        f"{d}/{m}/{str(y)[2:]}", f"{int(rng.integers(0, 24)):02d}:{int(rng.integers(0, 60)):02d}",
        f"{int(rng.integers(1, 13))}:{int(rng.integers(0, 60)):02d} {rng.choice(['AM', 'PM', 'a.m.', 'p.m.'])}",
    ]))


def _identifier(rng):
    letters = "".join(rng.choice(list(string.ascii_uppercase), size=int(rng.integers(1, 4))))
    return str(rng.choice([
        f"INV-{rng.integers(2000, 2040)}-{_digits(rng, 5)}", f"#{_digits(rng, int(rng.integers(3, 8)))}",
        f"{letters}{_digits(rng, 3)}-{_digits(rng, 2)}{letters[::-1]}",
        f"ISBN 978-{_digits(rng, 1)}-{_digits(rng, 2)}-{_digits(rng, 6)}-{_digits(rng, 1)}",
        f"No. {_digits(rng, int(rng.integers(1, 5)))}", f"Ref: {letters}/{_digits(rng, 4)}/{rng.integers(1, 99)}",
        f"{_digits(rng, 4)} {_digits(rng, 4)} {_digits(rng, 4)} {_digits(rng, 4)}",
        f"v{rng.integers(0, 10)}.{rng.integers(0, 30)}.{rng.integers(0, 100)}",
    ]))


def _contact(rng):
    name = "".join(rng.choice(list(string.ascii_lowercase), size=int(rng.integers(3, 9))))
    domain = "".join(rng.choice(list(string.ascii_lowercase), size=int(rng.integers(4, 10))))
    tld = TLDS[int(rng.integers(len(TLDS)))]
    return str(rng.choice([
        f"{name}@{domain}.{tld}", f"{name}.{domain[:4]}@{domain}.{tld}",
        f"https://www.{domain}.{tld}/{name}", f"www.{domain}.{tld}",
        f"+{rng.integers(1, 99)} ({_digits(rng, 3)}) {_digits(rng, 3)}-{_digits(rng, 4)}",
        f"+{rng.integers(1, 99)} {_digits(rng, 2)} {_digits(rng, 4)} {_digits(rng, 4)}",
        f"{_digits(rng, 3)}.{_digits(rng, 3)}.{_digits(rng, 4)}",
        f"{rng.integers(1, 9999)} {STREETS[int(rng.integers(len(STREETS)))]}, {CITIES[int(rng.integers(len(CITIES)))]} {_digits(rng, 5)}",
    ]))


def _heading(source, rng):
    roman = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII"]
    words = source.word_salad(rng)
    return str(rng.choice([
        f"CHAPTER {roman[int(rng.integers(len(roman)))]}", f"Section {rng.integers(1, 20)}.{rng.integers(1, 10)} — {words.title()}",
        f"Table {rng.integers(1, 30)}: {words.capitalize()}", f"Figure {rng.integers(1, 30)}.",
        f"Page {rng.integers(1, 40)} of {rng.integers(40, 400)}", f"- {rng.integers(1, 400)} -",
        words.upper(), f"{rng.integers(1, 20)}. {words.capitalize()}",
    ]))


def _list_item(source, rng):
    marker = str(rng.choice(["•", "–", "-", "*", "1.", "2.", "a)", "b)", "(i)", "(iv)", "§ 3", "¶"]))
    return f"{marker} {source.prose(rng, 50)}"


def _table_row(source, rng):
    cells = []
    for _ in range(int(rng.integers(2, 6))):
        kind = rng.random()
        if kind < 0.35:
            cells.append(_amount(rng))
        elif kind < 0.55:
            cells.append(str(rng.integers(0, 1000)))
        elif kind < 0.7:
            cells.append(_date(rng))
        else:
            cells.append(source.word_salad(rng).split(" ")[0])
    return " ".join(cells)


def _form_field(source, rng):
    label = LABELS[int(rng.integers(len(LABELS)))]
    value = str(rng.choice([_amount(rng), _date(rng), _identifier(rng), _contact(rng),
                            source.word_salad(rng), "_" * int(rng.integers(4, 16))]))
    return f"{label}{rng.choice([':', ': ', ' '])} {value}"


def _formula(source, rng):
    a, b = rng.integers(1, 100, size=2)
    return str(rng.choice([
        f"x = {a} + {b}y", f"f(x) = {a}x² + {b}x - 1", f"{a} × {b} = {a * b}", f"{a * b} ÷ {b} = {a}",
        f"±{rng.uniform(0, 5):.2f}", f"{rng.uniform(-40, 120):.1f} °C", f"{a}% / {b}‰",
        "½ + ¼ = ¾", "a <= b && c != d", f"if (n > {a}) {{ return [{b}]; }}", f"~{a}|{b}^2 \\ `{a}`",
        f"© {rng.integers(1990, 2040)} Example Corp.™", f"Brand® {a}µm",
    ]))


NAME_PARTS = ["pdf", "ops", "page", "tree", "max", "chars", "file", "name", "user", "id", "data",
              "text", "layout", "glyph", "font", "render", "core", "doc", "path", "out", "len",
              "index", "count", "size", "width", "image", "stream", "object", "ref", "cache",
              "config", "value", "key", "list", "map", "item", "indirect", "last", "error", "io"]


def _name(rng) -> str:
    """An identifier as code and technical writing spell them."""
    parts = [NAME_PARTS[int(rng.integers(len(NAME_PARTS)))] for _ in range(int(rng.integers(2, 4)))]
    style = rng.random()
    if style < 0.35:
        return "_".join(parts)
    if style < 0.6:
        return "".join(p.capitalize() for p in parts)  # ObjectId, IndirectObject
    if style < 0.7:
        return parts[0] + "".join(p.capitalize() for p in parts[1:])
    if style < 0.8:
        return "_".join(parts).upper()
    if style < 0.85:
        return f"__{parts[0]}__"
    return "_".join(parts) + str(rng.choice([".rs", ".py", ".dart", ".txt", ".pdf", ".json", ".md"]))


def _code(source, rng):
    """Identifiers, paths and calls: underscores, CamelCase (where I and l
    must be told apart by context) and slash-joined words."""
    n = lambda: _name(rng)  # noqa: E731
    words = source.word_salad(rng).split(" ")
    return str(rng.choice([
        f"{n()}::{n()}", f"{n()}({n()}, {n()})", f"let {n()} = {n()}.{n()}();",
        "/".join(n() for _ in range(int(rng.integers(2, 5)))), f"{n()}<{n()}, {n()}>",
        f"--{n().replace('_', '-')}", f"{n()}[{rng.integers(0, 100)}] = {n()};",
        f"C:\\Users\\{n()}\\{n()}.pdf", "/".join(words[:3]),
        f"{source.prose(rng, 30)} {n()} {source.prose(rng, 20)}",
    ]))


DOCUMENT_TEMPLATES = [
    lambda s, r: _amount(r), lambda s, r: _date(r), lambda s, r: _identifier(r),
    lambda s, r: _contact(r), _heading, _list_item, _table_row, _form_field, _formula,
    lambda s, r: f"{s.prose(r, 40)} {_amount(r)}", lambda s, r: f"{_date(r)} {s.prose(r, 40)}",
    _code, _code,
]
