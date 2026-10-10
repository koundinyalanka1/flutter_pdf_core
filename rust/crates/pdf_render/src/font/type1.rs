//! Type 1 font programs (`/FontFile`): the PostScript outline format that
//! pdfTeX, dvips and older Acrobat Distiller embed.
//!
//! Without this every LaTeX paper drew its Computer Modern and Times glyphs
//! with a substitute face — the wrong shapes, ligatures that turned into
//! gaps, and mathematical symbols that came out as unrelated letters.
//!
//! A program is a cleartext PostScript header (font name, matrix, built-in
//! encoding) followed by an `eexec`-encrypted private part holding the
//! subroutines and the per-glyph charstrings, each encrypted again. This
//! module decrypts both layers and interprets the Type 1 charstring language
//! (Adobe Type 1 Font Format, chapter 6), including flex, hint replacement
//! and `seac` accented characters.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::geom::Path;

const EEXEC_KEY: u16 = 55665;
const CHARSTRING_KEY: u16 = 4330;

/// A parsed Type 1 program.
pub struct Type1Font {
    /// Glyph name → charstring, decrypted and with its `lenIV` lead-in gone.
    charstrings: HashMap<String, Vec<u8>>,
    subrs: Vec<Vec<u8>>,
    /// The program's own encoding, code → glyph name.
    builtin_encoding: HashMap<u8, String>,
    /// Glyph units per em, from `/FontMatrix` — 1000 for nearly every font.
    pub units_per_em: f64,
}

impl Type1Font {
    /// Parse a program as embedded in `/FontFile`. `length1` is the stream's
    /// `/Length1`, the size of the cleartext part, when the document gives it.
    pub fn parse(data: &[u8], length1: Option<usize>) -> Option<Type1Font> {
        let data = strip_pfb_segments(data);
        let (clear, encrypted) = split_at_eexec(&data, length1)?;
        let private = eexec_decrypt(&binary_or_hex(encrypted));
        if private.len() < 4 {
            return None;
        }
        let private = &private[4..];

        let parsed = parse_private(private);
        let len_iv = parsed.len_iv;
        let decrypt = |raw: &[u8]| -> Vec<u8> {
            match usize::try_from(len_iv) {
                // lenIV -1 means the charstrings are stored in the clear.
                Err(_) => raw.to_vec(),
                Ok(skip) => {
                    let plain = decrypt(raw, CHARSTRING_KEY);
                    plain.get(skip..).map(<[u8]>::to_vec).unwrap_or_default()
                }
            }
        };
        let subrs: Vec<Vec<u8>> = parsed.subrs.iter().map(|s| decrypt(s)).collect();
        let charstrings: HashMap<String, Vec<u8>> = parsed
            .charstrings
            .into_iter()
            .map(|(name, raw)| (name, decrypt(&raw)))
            .collect();
        if charstrings.is_empty() {
            return None;
        }

        Some(Type1Font {
            charstrings,
            subrs,
            builtin_encoding: parse_builtin_encoding(clear),
            units_per_em: font_matrix_units(clear).unwrap_or(1000.0),
        })
    }

    pub fn has_glyph(&self, name: &str) -> bool {
        self.charstrings.contains_key(name)
    }

    /// The glyph name the program's built-in encoding gives `code`.
    pub fn builtin_name(&self, code: u8) -> Option<&str> {
        self.builtin_encoding.get(&code).map(String::as_str)
    }

    /// Outline of a glyph, in glyph units (y up).
    pub fn glyph_outline(&self, name: &str) -> Option<Path> {
        let mut machine = Machine::new(self);
        machine.draw(name, 0.0, 0.0, 0)?;
        machine.finish()
    }

    /// Advance width of a glyph, in glyph units, from its `hsbw`/`sbw`.
    pub fn advance(&self, name: &str) -> Option<f64> {
        let mut machine = Machine::new(self);
        machine.draw(name, 0.0, 0.0, 0)?;
        machine.advance
    }
}

// ---------------------------------------------------------------------------
// Container: PFB segments, the eexec boundary, decryption
// ---------------------------------------------------------------------------

/// Some writers embed the PFB file as-is: segments that each start with
/// `0x80`, a type byte and a little-endian length. Joining the payloads gives
/// the PFA layout everything else expects.
fn strip_pfb_segments(data: &[u8]) -> Cow<'_, [u8]> {
    if data.first() != Some(&0x80) {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos + 2 <= data.len() && data[pos] == 0x80 {
        let kind = data[pos + 1];
        if kind == 3 || pos + 6 > data.len() {
            break;
        }
        let length =
            u32::from_le_bytes([data[pos + 2], data[pos + 3], data[pos + 4], data[pos + 5]])
                as usize;
        let start = pos + 6;
        let end = start.saturating_add(length).min(data.len());
        out.extend_from_slice(&data[start..end]);
        pos = end;
    }
    Cow::Owned(out)
}

/// Split into the cleartext header and the encrypted part that follows the
/// `eexec` keyword.
fn split_at_eexec(data: &[u8], length1: Option<usize>) -> Option<(&[u8], &[u8])> {
    let keyword = find(data, b"eexec")?;
    let mut start = keyword + b"eexec".len();
    // /Length1 is exact when it is right; trust it only when it lands just
    // past the keyword, since writers get it wrong often enough.
    if let Some(length1) = length1 {
        if (start..=start + 4).contains(&length1) && length1 < data.len() {
            return Some((&data[..keyword], &data[length1..]));
        }
    }
    // Otherwise skip the end-of-line after the keyword, but no further: the
    // first encrypted byte may itself be a space, tab or newline value.
    match data.get(start..start + 2) {
        Some(b"\r\n") => start += 2,
        _ => {
            if matches!(data.get(start), Some(b'\r' | b'\n' | b' ' | b'\t')) {
                start += 1;
            }
        }
    }
    Some((&data[..keyword], data.get(start..)?))
}

/// The encrypted part is usually binary, but may be hexadecimal text. Four
/// leading hex digits are the spec's test for which.
fn binary_or_hex(encrypted: &[u8]) -> Cow<'_, [u8]> {
    let looks_hex = encrypted.len() >= 4 && encrypted[..4].iter().all(|b| b.is_ascii_hexdigit());
    if !looks_hex {
        return Cow::Borrowed(encrypted);
    }
    let digits: Vec<u8> = encrypted
        .iter()
        .copied()
        .filter(u8::is_ascii_hexdigit)
        .collect();
    Cow::Owned(
        digits
            .chunks_exact(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
                hi << 4 | lo
            })
            .collect(),
    )
}

fn eexec_decrypt(data: &[u8]) -> Vec<u8> {
    decrypt(data, EEXEC_KEY)
}

/// The Type 1 cipher (spec section 7.1): one shared by both layers.
fn decrypt(data: &[u8], key: u16) -> Vec<u8> {
    let mut r = key;
    data.iter()
        .map(|&cipher| {
            let plain = cipher ^ (r >> 8) as u8;
            r = (u16::from(cipher).wrapping_add(r))
                .wrapping_mul(52845)
                .wrapping_add(22719);
            plain
        })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// The private dictionary: lenIV, Subrs, CharStrings
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Private {
    len_iv: i64,
    subrs: Vec<Vec<u8>>,
    charstrings: Vec<(String, Vec<u8>)>,
}

/// A forgiving scanner over the decrypted private part. Binary charstring
/// data is only ever read where the syntax says it is — right after the
/// `RD`/`-|` token that introduces it — so it is never mistaken for tokens.
struct Scanner<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn skip_space(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if b == b'%' {
                while let Some(&c) = self.data.get(self.pos) {
                    self.pos += 1;
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                }
            } else if b.is_ascii_whitespace() || b == 0 {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// The next token: `/name`, a delimiter, or a run of regular characters.
    fn token(&mut self) -> Option<&'a [u8]> {
        self.skip_space();
        let start = self.pos;
        let first = *self.data.get(self.pos)?;
        self.pos += 1;
        if matches!(first, b'[' | b']' | b'{' | b'}') {
            return Some(&self.data[start..self.pos]);
        }
        while let Some(&b) = self.data.get(self.pos) {
            if b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'/' | b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'<' | b'>' | b'%'
                )
            {
                break;
            }
            self.pos += 1;
        }
        Some(&self.data[start..self.pos])
    }

    fn integer(&mut self) -> Option<i64> {
        std::str::from_utf8(self.token()?).ok()?.parse().ok()
    }

    /// `n RD <n bytes>`: the length, the token naming the read procedure,
    /// exactly one separator byte, then the data.
    fn binary(&mut self) -> Option<&'a [u8]> {
        let length = usize::try_from(self.integer()?).ok()?;
        self.token()?;
        let start = self.pos + 1;
        let end = start.checked_add(length)?;
        let bytes = self.data.get(start..end)?;
        self.pos = end;
        Some(bytes)
    }
}

fn parse_private(data: &[u8]) -> Private {
    let mut private = Private {
        len_iv: 4,
        ..Private::default()
    };
    let mut scanner = Scanner { data, pos: 0 };
    while let Some(token) = scanner.token() {
        match token {
            b"/lenIV" => {
                if let Some(value) = scanner.integer() {
                    private.len_iv = value;
                }
            }
            b"/Subrs" => {
                let count = scanner.integer().unwrap_or(0).clamp(0, 65_536) as usize;
                private.subrs = vec![Vec::new(); count];
                // `array`, then `dup i n RD <bytes> NP` per entry.
                loop {
                    let mark = scanner.pos;
                    match scanner.token() {
                        Some(b"dup") => {
                            let Some(index) = scanner.integer() else {
                                break;
                            };
                            let Some(bytes) = scanner.binary() else { break };
                            if let Some(slot) = usize::try_from(index)
                                .ok()
                                .and_then(|i| private.subrs.get_mut(i))
                            {
                                *slot = bytes.to_vec();
                            }
                        }
                        Some(b"array") | Some(b"NP") | Some(b"|") | Some(b"noaccess")
                        | Some(b"put") | Some(b"readonly") => {}
                        _ => {
                            scanner.pos = mark;
                            break;
                        }
                    }
                }
            }
            b"/CharStrings" => {
                // `n dict dup begin`, then `/name n RD <bytes> ND` until `end`.
                loop {
                    let mark = scanner.pos;
                    match scanner.token() {
                        Some(name) if name.starts_with(b"/") => {
                            let Some(bytes) = scanner.binary() else {
                                scanner.pos = mark;
                                break;
                            };
                            let name = String::from_utf8_lossy(&name[1..]).into_owned();
                            private.charstrings.push((name, bytes.to_vec()));
                        }
                        Some(b"end") | None => break,
                        Some(_) => {}
                    }
                }
            }
            _ => {}
        }
    }
    private
}

/// The cleartext `/Encoding`: either `StandardEncoding` or an array built
/// with `dup <code> /<name> put` entries.
fn parse_builtin_encoding(clear: &[u8]) -> HashMap<u8, String> {
    let mut encoding = HashMap::new();
    let Some(at) = find(clear, b"/Encoding") else {
        return encoding;
    };
    let mut scanner = Scanner {
        data: clear,
        pos: at + b"/Encoding".len(),
    };
    if scanner.token() == Some(b"StandardEncoding") {
        for code in 32u8..=126 {
            if let Some(name) = super::cff::standard_name_for_char(code as char) {
                encoding.insert(code, name.to_owned());
            }
        }
        return encoding;
    }
    // Walk to `readonly def` / `def`, collecting `dup code /name put`.
    let mut recent: Vec<&[u8]> = Vec::new();
    while let Some(token) = scanner.token() {
        if token == b"def" {
            break;
        }
        if token == b"put" && recent.len() >= 3 && recent[recent.len() - 3] == b"dup" {
            let code = std::str::from_utf8(recent[recent.len() - 2])
                .ok()
                .and_then(|s| s.parse::<u8>().ok());
            let name = recent[recent.len() - 1];
            if let (Some(code), Some(name)) = (code, name.strip_prefix(b"/")) {
                encoding.insert(code, String::from_utf8_lossy(name).into_owned());
            }
        }
        recent.push(token);
        if recent.len() > 4 {
            recent.remove(0);
        }
    }
    encoding
}

/// Glyph units per em from `/FontMatrix [a b c d e f]`: 1/a.
fn font_matrix_units(clear: &[u8]) -> Option<f64> {
    let at = find(clear, b"/FontMatrix")?;
    let mut scanner = Scanner {
        data: clear,
        pos: at + b"/FontMatrix".len(),
    };
    let open = scanner.token()?;
    if open != b"[" && open != b"{" {
        return None;
    }
    let a: f64 = std::str::from_utf8(scanner.token()?).ok()?.parse().ok()?;
    (a.is_finite() && a.abs() > 1e-9).then(|| 1.0 / a.abs())
}

// ---------------------------------------------------------------------------
// Charstring interpreter
// ---------------------------------------------------------------------------

/// StandardEncoding names for the codes `seac` takes its components by.
fn standard_name(code: f64) -> Option<&'static str> {
    let code = code as i64;
    if (32..=126).contains(&code) {
        return super::cff::standard_name_for_char(char::from(code as u8));
    }
    // The accented-letter components seac uses live in the high range.
    Some(match code {
        0xC1 => "grave",
        0xC2 => "acute",
        0xC3 => "circumflex",
        0xC4 => "tilde",
        0xC5 => "macron",
        0xC6 => "breve",
        0xC7 => "dotaccent",
        0xC8 => "dieresis",
        0xCA => "ring",
        0xCB => "cedilla",
        0xCD => "hungarumlaut",
        0xCE => "ogonek",
        0xCF => "caron",
        0xE1 => "AE",
        0xE8 => "Lslash",
        0xE9 => "Oslash",
        0xEA => "OE",
        0xF1 => "ae",
        0xF5 => "dotlessi",
        0xF8 => "lslash",
        0xF9 => "oslash",
        0xFA => "oe",
        0xFB => "germandbls",
        _ => return None,
    })
}

struct Machine<'f> {
    font: &'f Type1Font,
    path: Path,
    stack: Vec<f64>,
    /// The PostScript operand stack `callothersubr` and `pop` talk through.
    ps_stack: Vec<f64>,
    x: f64,
    y: f64,
    /// Origin offset for the glyph being drawn — non-zero for a seac accent.
    origin: (f64, f64),
    /// Points collected during a flex, the reference point first.
    flex: Option<Vec<(f64, f64)>>,
    open: bool,
    advance: Option<f64>,
    ended: bool,
}

impl<'f> Machine<'f> {
    fn new(font: &'f Type1Font) -> Self {
        Machine {
            font,
            path: Path::with_tolerance(font.units_per_em / 300.0),
            stack: Vec::with_capacity(24),
            ps_stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            origin: (0.0, 0.0),
            flex: None,
            open: false,
            advance: None,
            ended: false,
        }
    }

    /// Draw glyph `name` with its origin at `(dx, dy)`.
    fn draw(&mut self, name: &str, dx: f64, dy: f64, depth: usize) -> Option<()> {
        let charstring = self.font.charstrings.get(name)?;
        self.origin = (dx, dy);
        self.x = dx;
        self.y = dy;
        self.ended = false;
        self.run(charstring, depth);
        Some(())
    }

    fn finish(mut self) -> Option<Path> {
        if self.open {
            self.path.close();
        }
        (!self.path.is_empty()).then_some(self.path)
    }

    fn move_to(&mut self, x: f64, y: f64) {
        self.x = x;
        self.y = y;
        if self.flex.is_some() {
            // During flex a move only positions the next collected point.
            return;
        }
        if self.open {
            self.path.close();
        }
        self.path.move_to(x, y);
        self.open = true;
    }

    fn line_to(&mut self, x: f64, y: f64) {
        self.ensure_open();
        self.x = x;
        self.y = y;
        self.path.line_to(x, y);
    }

    fn curve_to(&mut self, points: [f64; 6]) {
        self.ensure_open();
        let [x1, y1, x2, y2, x3, y3] = points;
        self.path.curve_to(x1, y1, x2, y2, x3, y3);
        self.x = x3;
        self.y = y3;
    }

    /// A charstring may draw before any moveto; the spec's current point is
    /// the sidebearing point, so start the subpath there.
    fn ensure_open(&mut self) {
        if !self.open {
            self.path.move_to(self.x, self.y);
            self.open = true;
        }
    }

    fn run(&mut self, code: &[u8], depth: usize) {
        if depth > 10 {
            return;
        }
        let mut i = 0;
        while i < code.len() && !self.ended {
            let b = code[i];
            i += 1;
            match b {
                32..=246 => self.stack.push(f64::from(b) - 139.0),
                247..=250 => {
                    let Some(&w) = code.get(i) else { return };
                    i += 1;
                    self.stack
                        .push((f64::from(b) - 247.0) * 256.0 + f64::from(w) + 108.0);
                }
                251..=254 => {
                    let Some(&w) = code.get(i) else { return };
                    i += 1;
                    self.stack
                        .push(-(f64::from(b) - 251.0) * 256.0 - f64::from(w) - 108.0);
                }
                255 => {
                    let Some(bytes) = code.get(i..i + 4) else {
                        return;
                    };
                    i += 4;
                    self.stack.push(f64::from(i32::from_be_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3],
                    ])));
                }
                12 => {
                    let Some(&op) = code.get(i) else { return };
                    i += 1;
                    self.escape(op, depth);
                }
                _ => self.operator(b, depth),
            }
            if self.stack.len() > 64 {
                self.stack.clear();
            }
        }
    }

    fn arg(&self, index: usize) -> f64 {
        self.stack.get(index).copied().unwrap_or(0.0)
    }

    fn operator(&mut self, op: u8, depth: usize) {
        let s = &self.stack;
        let (x, y) = (self.x, self.y);
        match op {
            // hstem, vstem: hints, which a rasteriser without hinting ignores.
            1 | 3 => {}
            4 => self.move_to(x, y + self.arg(0)),
            5 => self.line_to(x + self.arg(0), y + self.arg(1)),
            6 => self.line_to(x + self.arg(0), y),
            7 => self.line_to(x, y + self.arg(0)),
            8 if s.len() >= 6 => {
                let (x1, y1) = (x + s[0], y + s[1]);
                let (x2, y2) = (x1 + s[2], y1 + s[3]);
                self.curve_to([x1, y1, x2, y2, x2 + s[4], y2 + s[5]]);
            }
            9 => {
                if self.open {
                    self.path.close();
                    self.open = false;
                }
            }
            10 => {
                let index = self.stack.pop().unwrap_or(-1.0);
                if let Some(subr) = usize::try_from(index as i64)
                    .ok()
                    .and_then(|i| self.font.subrs.get(i))
                {
                    let subr = subr.clone();
                    // Subroutine operands stay on the stack for the caller.
                    self.run(&subr, depth + 1);
                }
                return;
            }
            // return: leave the subroutine, keeping the stack.
            11 => return,
            13 => {
                // hsbw: sidebearing and advance; the sidebearing point is the
                // current point.
                self.x = self.origin.0 + self.arg(0);
                self.y = self.origin.1;
                self.advance.get_or_insert(self.arg(1));
            }
            14 => {
                if self.open {
                    self.path.close();
                    self.open = false;
                }
                self.ended = true;
            }
            21 => self.move_to(x + self.arg(0), y + self.arg(1)),
            22 => self.move_to(x + self.arg(0), y),
            30 if s.len() >= 4 => {
                let (x1, y1) = (x, y + s[0]);
                let (x2, y2) = (x1 + s[1], y1 + s[2]);
                self.curve_to([x1, y1, x2, y2, x2 + s[3], y2]);
            }
            31 if s.len() >= 4 => {
                let (x1, y1) = (x + s[0], y);
                let (x2, y2) = (x1 + s[1], y1 + s[2]);
                self.curve_to([x1, y1, x2, y2, x2, y2 + s[3]]);
            }
            _ => {}
        }
        self.stack.clear();
    }

    fn escape(&mut self, op: u8, depth: usize) {
        match op {
            // dotsection, vstem3, hstem3: hints.
            0..=2 => {}
            6 => {
                // seac: an accented character from two standard glyphs.
                let (asb, adx, ady, bchar, achar) = (
                    self.arg(0),
                    self.arg(1),
                    self.arg(2),
                    self.arg(3),
                    self.arg(4),
                );
                self.stack.clear();
                if depth == 0 {
                    let advance = self.advance;
                    if let Some(base) = standard_name(bchar) {
                        self.draw(base, 0.0, 0.0, depth + 1);
                    }
                    if let Some(accent) = standard_name(achar) {
                        // The accent's own hsbw adds its sidebearing back, so
                        // its sidebearing point lands at (adx, ady).
                        self.draw(accent, adx - asb, ady, depth + 1);
                    }
                    self.advance = advance;
                }
                self.ended = true;
                return;
            }
            7 => {
                // sbw: like hsbw, with a vertical component.
                self.x = self.origin.0 + self.arg(0);
                self.y = self.origin.1 + self.arg(1);
                self.advance.get_or_insert(self.arg(2));
            }
            12 => {
                // div: the one arithmetic operator, for numbers too large to
                // encode directly.
                let b = self.stack.pop().unwrap_or(1.0);
                let a = self.stack.pop().unwrap_or(0.0);
                self.stack.push(if b == 0.0 { 0.0 } else { a / b });
                return;
            }
            16 => {
                self.call_other_subr();
                return;
            }
            17 => {
                // pop: move a value from the PostScript stack back.
                let value = self.ps_stack.pop().unwrap_or(0.0);
                self.stack.push(value);
                return;
            }
            33 => {
                // setcurrentpoint, after flex: absolute character-space
                // coordinates, so a seac accent's offset still applies.
                self.x = self.origin.0 + self.arg(0);
                self.y = self.origin.1 + self.arg(1);
            }
            _ => {}
        }
        self.stack.clear();
    }

    /// `arg1 … argn n othersubr# callothersubr`. Subrs 0–2 implement flex,
    /// 3 hint replacement; anything else hands its arguments to the
    /// PostScript stack untouched so the `pop`s that follow find them.
    fn call_other_subr(&mut self) {
        let number = self.stack.pop().unwrap_or(-1.0) as i64;
        let count = (self.stack.pop().unwrap_or(0.0).max(0.0) as usize).min(self.stack.len());
        let args = self.stack.split_off(self.stack.len() - count);
        match number {
            1 => self.flex = Some(Vec::with_capacity(7)),
            2 => {
                if let Some(points) = self.flex.as_mut() {
                    points.push((self.x, self.y));
                }
            }
            0 => {
                let points = self.flex.take().unwrap_or_default();
                if points.len() >= 7 {
                    // points[0] is the reference point; two curves follow.
                    let p = &points;
                    self.curve_to([p[1].0, p[1].1, p[2].0, p[2].1, p[3].0, p[3].1]);
                    self.curve_to([p[4].0, p[4].1, p[5].0, p[5].1, p[6].0, p[6].1]);
                }
                // `pop pop setcurrentpoint` follows: hand back x, then y.
                let end_y = args.get(2).copied().unwrap_or(self.y);
                let end_x = args.get(1).copied().unwrap_or(self.x);
                self.ps_stack.push(end_y);
                self.ps_stack.push(end_x);
            }
            _ => {
                for &value in args.iter().rev() {
                    self.ps_stack.push(value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt with the Type 1 cipher — the inverse of [`decrypt`] — so tests
    /// can build programs the way a font tool would.
    fn encrypt(plain: &[u8], key: u16) -> Vec<u8> {
        let mut r = key;
        plain
            .iter()
            .map(|&p| {
                let cipher = p ^ (r >> 8) as u8;
                r = (u16::from(cipher).wrapping_add(r))
                    .wrapping_mul(52845)
                    .wrapping_add(22719);
                cipher
            })
            .collect()
    }

    fn number(v: i32) -> Vec<u8> {
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => {
                let v = v - 108;
                vec![(v / 256 + 247) as u8, (v % 256) as u8]
            }
            -1131..=-108 => {
                let v = -v - 108;
                vec![(v / 256 + 251) as u8, (v % 256) as u8]
            }
            _ => {
                let mut out = vec![255];
                out.extend_from_slice(&v.to_be_bytes());
                out
            }
        }
    }

    fn charstring(ops: &[(&[i32], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in ops {
            for &a in *args {
                out.extend(number(a));
            }
            out.extend_from_slice(op);
        }
        out
    }

    /// A program with the given glyphs and subrs, cleartext + eexec.
    fn program(glyphs: &[(&str, Vec<u8>)], subrs: &[Vec<u8>], encoding: &str) -> Vec<u8> {
        let mut private = b"dup /Private 8 dict dup begin /lenIV 4 def\n".to_vec();
        private.extend(format!("/Subrs {} array\n", subrs.len()).bytes());
        for (i, subr) in subrs.iter().enumerate() {
            let mut body = vec![0, 0, 0, 0];
            body.extend_from_slice(subr);
            let enc = encrypt(&body, CHARSTRING_KEY);
            private.extend(format!("dup {i} {} RD ", enc.len()).bytes());
            private.extend(enc);
            private.extend(b" NP\n");
        }
        private.extend(format!("2 index /CharStrings {} dict dup begin\n", glyphs.len()).bytes());
        for (name, cs) in glyphs {
            let mut body = vec![0, 0, 0, 0];
            body.extend_from_slice(cs);
            let enc = encrypt(&body, CHARSTRING_KEY);
            private.extend(format!("/{name} {} RD ", enc.len()).bytes());
            private.extend(enc);
            private.extend(b" ND\n");
        }
        private.extend(b"end\nend\nreadonly put\nput\n");
        let mut plain = b"\0\0\0\0".to_vec();
        plain.extend(private);

        let mut out = format!(
            "%!PS-AdobeFont-1.0: Test\n/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n{encoding}\ncurrentfile eexec\n"
        )
        .into_bytes();
        out.extend(encrypt(&plain, EEXEC_KEY));
        out
    }

    fn square() -> Vec<u8> {
        // hsbw 50 600; rmoveto 0 0; 500 right, 500 up, 500 left; closepath.
        charstring(&[
            (&[50, 600], &[13]),
            (&[0, 0], &[21]),
            (&[500], &[6]),
            (&[500], &[7]),
            (&[-500], &[6]),
            (&[], &[9]),
            (&[], &[14]),
        ])
    }

    #[test]
    fn decrypts_and_draws_a_glyph() {
        let data = program(&[("A", square())], &[], "/Encoding StandardEncoding def");
        let font = Type1Font::parse(&data, None).expect("program parses");
        assert_eq!(font.units_per_em, 1000.0);
        assert_eq!(font.builtin_name(65), Some("A"));
        let outline = font.glyph_outline("A").expect("outline");
        assert_eq!(outline.bounds(), Some((50.0, 0.0, 550.0, 500.0)));
        assert_eq!(font.advance("A"), Some(600.0));
    }

    #[test]
    fn custom_encoding_arrays_are_read() {
        let encoding = "/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\ndup 12 /fi put\ndup 65 /alpha put\nreadonly def";
        let data = program(&[("fi", square()), ("alpha", square())], &[], encoding);
        let font = Type1Font::parse(&data, None).unwrap();
        assert_eq!(font.builtin_name(12), Some("fi"));
        assert_eq!(font.builtin_name(65), Some("alpha"));
        assert!(font.has_glyph("fi"));
    }

    #[test]
    fn subroutines_are_called_and_return() {
        // Subr 4 draws the right and top edges.
        let subr = charstring(&[(&[500], &[6]), (&[500], &[7]), (&[], &[11])]);
        let glyph = charstring(&[
            (&[0, 600], &[13]),
            (&[0, 0], &[21]),
            (&[4], &[10]),
            (&[-500], &[6]),
            (&[], &[9]),
            (&[], &[14]),
        ]);
        let subrs = vec![vec![11], vec![11], vec![11], vec![11], subr];
        let font = Type1Font::parse(&program(&[("A", glyph)], &subrs, ""), None).unwrap();
        assert_eq!(
            font.glyph_outline("A").unwrap().bounds(),
            Some((0.0, 0.0, 500.0, 500.0))
        );
    }

    /// Flex: seven points collected through othersubrs 1 and 2 become two
    /// curves when othersubr 0 ends it, as in every hinted CM glyph.
    #[test]
    fn flex_becomes_two_curves() {
        let mut ops: Vec<(&[i32], &[u8])> = vec![(&[0, 600], &[13]), (&[0, 0], &[21])];
        ops.push((&[0, 1], &[12, 16])); // othersubr 1: start flex
        let moves: [[i32; 2]; 7] = [
            [100, 0], // reference point
            [0, 50],
            [50, 0],
            [50, 0],
            [50, 0],
            [50, 0],
            [0, -50],
        ];
        let mut out = charstring(&ops);
        for [dx, dy] in moves {
            out.extend(charstring(&[(&[dx, dy], &[21]), (&[0, 2], &[12, 16])]));
        }
        // flexheight endx endy 3 0 callothersubr pop pop setcurrentpoint
        out.extend(charstring(&[
            (&[50, 300, 0, 3, 0], &[12, 16]),
            (&[], &[12, 17]),
            (&[], &[12, 17]),
            (&[], &[12, 33]),
            (&[], &[9]),
            (&[], &[14]),
        ]));
        let font = Type1Font::parse(&program(&[("A", out)], &[], ""), None).unwrap();
        let bounds = font.glyph_outline("A").unwrap().bounds().unwrap();
        // The curves reach x = 300 and rise to y = 50 before coming back.
        assert!((bounds.2 - 300.0).abs() < 1.0, "{bounds:?}");
        assert!(bounds.3 > 20.0, "{bounds:?}");
    }

    #[test]
    fn seac_composes_base_and_accent() {
        // Base "A" is the square; accent "acute" is a small square on top.
        let accent = charstring(&[
            (&[100, 300], &[13]),
            (&[0, 600], &[21]),
            (&[100], &[6]),
            (&[100], &[7]),
            (&[-100], &[6]),
            (&[], &[9]),
            (&[], &[14]),
        ]);
        // asb adx ady bchar achar seac — acute is StandardEncoding 0xC2.
        let aacute = charstring(&[(&[50, 600], &[13]), (&[100, 300, 0, 65, 0xC2], &[12, 6])]);
        let data = program(
            &[("A", square()), ("acute", accent), ("Aacute", aacute)],
            &[],
            "/Encoding StandardEncoding def",
        );
        let font = Type1Font::parse(&data, None).unwrap();
        let bounds = font.glyph_outline("Aacute").unwrap().bounds().unwrap();
        assert_eq!(bounds.1, 0.0, "base reaches the baseline");
        assert_eq!(bounds.3, 700.0, "accent sits above it");
        // The accent's sidebearing point lands at adx = 300.
        assert_eq!(
            font.glyph_outline("acute").unwrap().bounds().unwrap().0,
            100.0
        );
        assert_eq!(font.advance("Aacute"), Some(600.0));
    }

    #[test]
    fn hex_eexec_and_pfb_segments_are_accepted() {
        let binary = program(&[("A", square())], &[], "");
        let at = find(&binary, b"eexec").unwrap() + "eexec\n".len();
        // Hex form of the encrypted part.
        let mut hex = binary[..at].to_vec();
        for chunk in binary[at..].chunks(32) {
            for b in chunk {
                hex.extend(format!("{b:02x}").bytes());
            }
            hex.push(b'\n');
        }
        assert!(Type1Font::parse(&hex, None).unwrap().has_glyph("A"));
        // PFB: an ASCII segment, a binary segment, an EOF marker.
        let mut pfb = vec![0x80, 1];
        pfb.extend((at as u32).to_le_bytes());
        pfb.extend_from_slice(&binary[..at]);
        pfb.extend([0x80, 2]);
        pfb.extend(((binary.len() - at) as u32).to_le_bytes());
        pfb.extend_from_slice(&binary[at..]);
        pfb.extend([0x80, 3]);
        assert!(Type1Font::parse(&pfb, None).unwrap().has_glyph("A"));
    }

    #[test]
    fn length1_marks_the_encrypted_start() {
        let data = program(&[("A", square())], &[], "");
        let at = find(&data, b"eexec").unwrap() + "eexec\n".len();
        assert!(Type1Font::parse(&data, Some(at)).unwrap().has_glyph("A"));
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(Type1Font::parse(b"not a font", None).is_none());
        assert!(Type1Font::parse(b"currentfile eexec \x01\x02", None).is_none());
        assert!(Type1Font::parse(&[0x80, 1, 0xFF, 0xFF, 0xFF, 0x7F], None).is_none());
    }
}
