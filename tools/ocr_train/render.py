"""Synthetic text lines that look like crops from scanned or rendered pages.

The crop comes from the *clean* ink box of the drawn text, jittered the way
line detection is imprecise, then normalized exactly as the engine does
(`normalize.py`). Degradations imitate scanners and phone cameras: skew,
blur, resolution loss, noise, bitonal thresholding, ink spread or thinning,
JPEG artefacts, uneven lighting and neighbouring lines bleeding into the
padding.
"""

import io
import math
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

import numpy as np
from fontTools.ttLib import TTCollection, TTFont
from PIL import Image, ImageDraw, ImageFilter, ImageFont

from charset import CHARSET
from normalize import crop_rect, normalize

CHARSET_SET = set(CHARSET)


@dataclass(frozen=True)
class Face:
    path: str
    index: int  # face index inside a .ttc collection
    family: str
    chars: frozenset
    axes: tuple  # ((tag, min, default, max), ...) for variable fonts


def scan_faces(paths) -> list[Face]:
    """Read each font's character coverage once (fontTools), skipping fonts
    that cannot render the basic Latin alphabet."""
    faces = []
    for path in paths:
        path = str(path)
        try:
            if path.lower().endswith((".ttc", ".otc")):
                fonts = list(TTCollection(path, lazy=True).fonts)
            else:
                fonts = [TTFont(path, lazy=True)]
        except Exception:
            continue
        for index, font in enumerate(fonts):
            try:
                cmap = font.getBestCmap() or {}
            except Exception:
                continue
            chars = frozenset(chr(c) for c in cmap if chr(c) in CHARSET_SET)
            if not set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.,") <= chars:
                continue
            axes = ()
            if "fvar" in font:
                axes = tuple((a.axisTag, a.minValue, a.defaultValue, a.maxValue) for a in font["fvar"].axes)
            family = Path(path).parent.name if "google-fonts" in path else Path(path).stem.split(" ")[0]
            faces.append(Face(path, index, family, chars, axes))
    return faces


@lru_cache(maxsize=4096)
def _font(path: str, index: int, size: int, raqm: bool) -> ImageFont.FreeTypeFont:
    engine = ImageFont.Layout.RAQM if raqm else ImageFont.Layout.BASIC
    return ImageFont.truetype(path, size=size, index=index, layout_engine=engine)


def _set_variation(font: ImageFont.FreeTypeFont, face: Face, rng) -> None:
    if not face.axes:
        return
    values = []
    for tag, lo, default, hi in face.axes:
        value = default
        if tag == "wght":
            choice = rng.random()
            if choice < 0.25:
                value = 700
            elif choice < 0.4:
                value = rng.uniform(lo, hi)
        elif tag == "wdth" and rng.random() < 0.2:
            value = rng.uniform(lo, hi)
        values.append(float(min(max(value, lo), hi)))
    try:
        font.set_variation_by_axes(values)
    except Exception:
        pass


class LineRenderer:
    def __init__(self, faces: list[Face], degrade: float = 1.0):
        self.by_family: dict[str, list[Face]] = {}
        for face in faces:
            self.by_family.setdefault(face.family, []).append(face)
        self.families = sorted(self.by_family)
        self.degrade = degrade  # 0 = clean renders (evaluation), 1 = full augmentation

    def pick_face(self, text: str, rng):
        needed = set(text) - {" "}
        for _ in range(30):
            family = self.by_family[self.families[int(rng.integers(len(self.families)))]]
            face = family[int(rng.integers(len(family)))]
            if needed <= face.chars:
                return face
        return None

    def render(self, text: str, rng, neighbour=None):
        """Normalized float32 [32, W] line image for `text`, or None."""
        try:
            return self._render(text, rng, neighbour)
        except (OSError, ValueError):
            # FreeType rejects some faces at some sizes (hinting stack
            # overflows, broken tables); skip the sample, not the run.
            return None

    def _render(self, text: str, rng, neighbour):
        face = self.pick_face(text, rng)
        if face is None:
            return None
        size = int(rng.integers(12, 18)) if rng.random() < 0.1 else int(rng.integers(18, 57))
        font = _font(face.path, face.index, size, bool(rng.random() < 0.75))
        _set_variation(font, face, rng)

        ascent, descent = font.getmetrics()
        space = font.getlength(" ") or size * 0.25
        gap = float(rng.choice([1.0, rng.uniform(0.7, 1.6), rng.uniform(1.6, 4.5)], p=[0.6, 0.3, 0.1]))
        tracking = rng.uniform(-0.06, 0.22) * size if rng.random() < 0.12 else 0.0
        margin = int(size * 0.8) + 4  # crop padding, jitter and slant
        words = text.split(" ")
        positions, x, text_end = [], float(margin), float(margin)
        for word in words:
            positions.append(x)
            text_end = x + self._advance(font, word, tracking)
            x = text_end + space * gap * rng.uniform(0.85, 1.15)
        width = int(text_end) + margin
        # Room above and below for the padding, neighbouring lines and the
        # line ends rising or falling when the line is skewed (up to 2.5°).
        margin_y = int(max(0.7 * size, 0.045 * width)) + 4
        height = ascent + descent + 2 * margin_y
        baseline = margin_y + ascent

        background = int(rng.integers(235, 256)) if rng.random() < 0.8 else int(rng.integers(185, 235))
        ink = int(rng.integers(0, 70)) if rng.random() < 0.92 else int(rng.integers(70, 130))
        ink = min(ink, background - 60)
        # Draw coverage once; the picture is paper blended towards ink by it,
        # which is exactly what drawing in the ink colour would produce.
        mask = Image.new("L", (width, height), 0)
        draw = ImageDraw.Draw(mask)
        for word, x in zip(words, positions):
            self._draw_word(draw, font, word, x, baseline, tracking, 255)
        if rng.random() < 0.04 * self.degrade:
            y = baseline + max(1, descent // 3)
            thickness = max(1, size // 16)
            draw.rectangle([positions[0], y, max(positions[0], text_end), y + thickness - 1], fill=255)
        coverage = np.asarray(mask, dtype=np.float32)
        if neighbour is not None and rng.random() < 0.15 * self.degrade:
            others = Image.new("L", (width, height), 0)
            other_draw = ImageDraw.Draw(others)
            leading = (ascent + descent) * rng.uniform(1.0, 1.35)
            for direction in (-1, 1):
                other = neighbour()
                if rng.random() < 0.6 and set(other) - {" "} <= face.chars:
                    self._draw_word(other_draw, font, other, float(margin) + rng.uniform(-size, size),
                                    baseline + direction * leading, 0.0, 255)
            coverage = np.maximum(coverage, np.asarray(others, dtype=np.float32))
        picture = background + (ink - background) * (coverage / 255.0)
        image = Image.fromarray(np.rint(picture).astype(np.uint8))

        image, mask = self._geometry(image, mask, background, rng)
        box = mask.getbbox()
        if box is None:
            return None
        # Degrade only what the engine will look at: the padded line crop.
        crop = crop_rect(self._jitter(box, rng), image.width, image.height)
        pixels = self._photometric(image.crop(crop), background, ink, size, rng)
        return normalize(pixels, (0, 0, pixels.shape[1], pixels.shape[0]))

    # -- drawing -------------------------------------------------------------

    @staticmethod
    def _advance(font, word: str, tracking: float) -> float:
        if tracking == 0.0:
            return font.getlength(word)
        return sum(font.getlength(ch) + tracking for ch in word)

    @staticmethod
    def _draw_word(draw, font, word, x, baseline, tracking, fill):
        if tracking == 0.0:
            draw.text((x, baseline), word, font=font, fill=fill, anchor="ls")
            return
        for ch in word:
            draw.text((x, baseline), ch, font=font, fill=fill, anchor="ls")
            x += font.getlength(ch) + tracking

    # -- degradations --------------------------------------------------------

    def _geometry(self, image, mask, background, rng):
        angle = 0.0
        if rng.random() < 0.5 * self.degrade:
            angle = float(np.clip(rng.normal(0.0, 0.7), -2.5, 2.5))
        shear = float(rng.uniform(-0.12, 0.12)) if rng.random() < 0.08 * self.degrade else 0.0
        if angle == 0.0 and shear == 0.0:
            return image, mask
        w, h = image.size
        a = math.radians(angle)
        cos, sin = math.cos(a), math.sin(a)
        cx, cy = w / 2, h / 2
        # Inverse map (output -> input) for PIL: rotation about the centre,
        # then horizontal shear.
        coeffs = (cos, sin + shear, cx - cos * cx - (sin + shear) * cy,
                  -sin, cos, cy + sin * cx - cos * cy)
        image = image.transform((w, h), Image.Transform.AFFINE, coeffs,
                                resample=Image.Resampling.BICUBIC, fillcolor=background)
        mask = mask.transform((w, h), Image.Transform.AFFINE, coeffs,
                              resample=Image.Resampling.BILINEAR, fillcolor=0)
        return image, mask.point(lambda v: 255 if v > 96 else 0)

    def _photometric(self, image, background, ink, size, rng) -> np.ndarray:
        d = self.degrade
        w, h = image.size
        if rng.random() < 0.3 * d:  # lost resolution, then resampled back up
            f = rng.uniform(0.3, 0.8)
            small = image.resize((max(1, int(w * f)), max(1, int(h * f))), Image.Resampling.BOX)
            image = small.resize((w, h), Image.Resampling.BILINEAR)
        if rng.random() < 0.35 * d:
            image = image.filter(ImageFilter.GaussianBlur(rng.uniform(0.3, 1.4) * size / 32))
        if size >= 28 and rng.random() < 0.06 * d:
            image = image.filter(ImageFilter.MinFilter(3))  # ink spreads
        elif size >= 28 and rng.random() < 0.06 * d:
            image = image.filter(ImageFilter.MaxFilter(3))  # ink thins
        pixels = np.asarray(image, dtype=np.float32)
        if rng.random() < 0.08 * d:  # uneven lighting across the line
            ramp = np.linspace(rng.uniform(0.7, 1.0), rng.uniform(0.7, 1.0), w, dtype=np.float32)
            pixels = pixels * ramp[None, :]
        if rng.random() < 0.08 * d:  # paper texture
            coarse = rng.normal(0, rng.uniform(4, 12), size=(max(1, h // 8), max(1, w // 8)))
            texture = Image.fromarray(coarse.astype(np.float32)).resize((w, h), Image.Resampling.BILINEAR)
            pixels = pixels + np.asarray(texture)
        if rng.random() < 0.4 * d:
            pixels = pixels + rng.normal(0, rng.uniform(2, 16), size=pixels.shape)
        if rng.random() < 0.1 * d:
            speckle = rng.random(pixels.shape) < rng.uniform(0.0005, 0.006)
            pixels[speckle] = rng.choice([0.0, 255.0], size=int(speckle.sum()))
        if rng.random() < 0.15 * d:
            gamma = rng.uniform(0.6, 1.5)
            pixels = 255.0 * (np.clip(pixels, 0, 255) / 255.0) ** gamma
        pixels = np.clip(pixels, 0, 255).astype(np.uint8)
        if rng.random() < 0.12 * d:  # bitonal scan
            threshold = (background + ink) / 2 + rng.normal(0, 15)
            pixels = np.where(pixels < threshold, 0, 255).astype(np.uint8)
        if rng.random() < 0.25 * d:
            buffer = io.BytesIO()
            Image.fromarray(pixels).save(buffer, format="JPEG", quality=int(rng.integers(25, 86)))
            pixels = np.asarray(Image.open(io.BytesIO(buffer.getvalue())).convert("L"))
        return pixels

    def _jitter(self, box, rng):
        x0, y0, x1, y1 = (float(v) for v in box)
        h = y1 - y0
        sigma = 0.03 if rng.random() < 0.9 else 0.08
        sigma *= self.degrade
        x0 += rng.normal(0, sigma) * h
        x1 += rng.normal(0, sigma) * h
        y0 += rng.normal(0, sigma) * h
        y1 += rng.normal(0, sigma) * h
        if x1 - x0 < 2 or y1 - y0 < 2:
            return tuple(float(v) for v in box)
        return x0, y0, x1, y1
