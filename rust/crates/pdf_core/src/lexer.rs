use crate::error::{PdfError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Name(String),
    LiteralString(Vec<u8>),
    HexString(Vec<u8>),
    Keyword(Keyword),
    ArrayStart,
    ArrayEnd,
    DictStart,
    DictEnd,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub offset: usize,
}

pub struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn with_offset(data: &'a [u8], offset: usize) -> Self {
        Self { data, pos: offset }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    /// Read stream payload bytes. When `length_hint` (from a direct
    /// `/Length`) is plausible — i.e. `endstream` follows it — the exact
    /// length is used, which keeps binary payloads containing the word
    /// "endstream" intact. Otherwise falls back to scanning.
    pub fn read_stream_data_with_length(
        &mut self,
        stream_keyword_offset: usize,
        length_hint: Option<usize>,
    ) -> Result<Vec<u8>> {
        if self.pos < self.data.len() && self.data[self.pos] == b'\r' {
            self.pos += 1;
            if self.pos < self.data.len() && self.data[self.pos] == b'\n' {
                self.pos += 1;
            }
        } else if self.pos < self.data.len() && self.data[self.pos] == b'\n' {
            self.pos += 1;
        }
        let start = self.pos;

        if let Some(len) = length_hint {
            let data_end = start.checked_add(len).unwrap_or(usize::MAX);
            if data_end <= self.data.len() {
                // Allow EOL + optional whitespace before `endstream`.
                let mut probe = data_end;
                while probe < self.data.len() && is_ws(self.data[probe]) && probe < data_end + 4 {
                    probe += 1;
                }
                if self.data[probe..].starts_with(b"endstream") {
                    let data = self.data[start..data_end].to_vec();
                    self.pos = probe + b"endstream".len();
                    return Ok(data);
                }
            }
        }
        self.pos = start;
        self.scan_stream_data(stream_keyword_offset, start)
    }

    pub fn read_stream_data(&mut self, stream_keyword_offset: usize) -> Result<Vec<u8>> {
        if self.pos < self.data.len() && self.data[self.pos] == b'\r' {
            self.pos += 1;
            if self.pos < self.data.len() && self.data[self.pos] == b'\n' {
                self.pos += 1;
            }
        } else if self.pos < self.data.len() && self.data[self.pos] == b'\n' {
            self.pos += 1;
        }
        let start = self.pos;
        self.scan_stream_data(stream_keyword_offset, start)
    }

    fn scan_stream_data(&mut self, stream_keyword_offset: usize, start: usize) -> Result<Vec<u8>> {
        let marker = b"endstream";
        let Some(marker_offset) = self.data[start..]
            .windows(marker.len())
            .position(|window| window == marker)
            .map(|relative| start + relative)
        else {
            // The search read everything after the stream keyword, and the
            // position says so: callers measure a failed parse by it.
            self.pos = self.data.len();
            return Err(PdfError::parse(
                stream_keyword_offset,
                "unterminated stream",
            ));
        };

        let mut end = marker_offset;
        if end >= start + 2 && &self.data[end - 2..end] == b"\r\n" {
            end -= 2;
        } else if end > start && matches!(self.data[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        let data = self.data[start..end].to_vec();
        self.pos = marker_offset + marker.len();
        Ok(data)
    }

    pub fn next_token(&mut self) -> Result<Option<SpannedToken>> {
        self.skip_ws_and_comments();
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let offset = self.pos;
        let token = match self.peek() {
            b'[' => {
                self.pos += 1;
                Token::ArrayStart
            }
            b']' => {
                self.pos += 1;
                Token::ArrayEnd
            }
            b'<' if self.peek_next() == Some(b'<') => {
                self.pos += 2;
                Token::DictStart
            }
            b'>' if self.peek_next() == Some(b'>') => {
                self.pos += 2;
                Token::DictEnd
            }
            b'<' => self.hex_string()?,
            b'/' => self.name()?,
            b'(' => self.literal_string()?,
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.number()?,
            _ => self.keyword()?,
        };
        Ok(Some(SpannedToken { token, offset }))
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while self.pos < self.data.len() && is_ws(self.data[self.pos]) {
                self.pos += 1;
            }
            if self.pos < self.data.len() && self.data[self.pos] == b'%' {
                while self.pos < self.data.len()
                    && self.data[self.pos] != b'\n'
                    && self.data[self.pos] != b'\r'
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> u8 {
        self.data[self.pos]
    }

    fn peek_next(&self) -> Option<u8> {
        self.data.get(self.pos + 1).copied()
    }

    fn name(&mut self) -> Result<Token> {
        self.pos += 1;
        let mut bytes = Vec::new();
        while self.pos < self.data.len()
            && !is_delimiter(self.data[self.pos])
            && !is_ws(self.data[self.pos])
        {
            if self.data[self.pos] == b'#' && self.pos + 2 < self.data.len() {
                if let (Some(hi), Some(lo)) = (
                    hex_value(self.data[self.pos + 1]),
                    hex_value(self.data[self.pos + 2]),
                ) {
                    bytes.push((hi << 4) | lo);
                    self.pos += 3;
                    continue;
                }
            }
            bytes.push(self.data[self.pos]);
            self.pos += 1;
        }
        Ok(Token::Name(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn literal_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.pos += 1;
        let mut depth = 1usize;
        let mut out = Vec::new();
        while self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            match b {
                b'\\' => {
                    if self.pos >= self.data.len() {
                        return Err(PdfError::parse(
                            start,
                            "unterminated escape in literal string",
                        ));
                    }
                    let esc = self.data[self.pos];
                    self.pos += 1;
                    match esc {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'(' | b')' | b'\\' => out.push(esc),
                        b'\r' => {
                            if self.pos < self.data.len() && self.data[self.pos] == b'\n' {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut value = esc - b'0';
                            for _ in 0..2 {
                                if self.pos < self.data.len()
                                    && matches!(self.data[self.pos], b'0'..=b'7')
                                {
                                    value = value * 8 + (self.data[self.pos] - b'0');
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                            out.push(value);
                        }
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Token::LiteralString(out));
                    }
                    out.push(b);
                }
                other => out.push(other),
            }
        }
        Err(PdfError::parse(start, "unterminated literal string"))
    }

    fn hex_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.pos += 1;
        let mut nibbles = Vec::new();
        while self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            if b == b'>' {
                if nibbles.len() % 2 == 1 {
                    nibbles.push(0);
                }
                let bytes = nibbles
                    .chunks(2)
                    .map(|pair| (pair[0] << 4) | pair[1])
                    .collect();
                return Ok(Token::HexString(bytes));
            }
            if is_ws(b) {
                continue;
            }
            if let Some(v) = hex_value(b) {
                nibbles.push(v);
            } else {
                return Err(PdfError::parse(self.pos - 1, "invalid hex digit"));
            }
        }
        Err(PdfError::parse(start, "unterminated hex string"))
    }

    fn number(&mut self) -> Result<Token> {
        let start = self.pos;
        if matches!(self.peek(), b'+' | b'-') {
            self.pos += 1;
        }
        let mut has_dot = false;
        while self.pos < self.data.len() {
            match self.data[self.pos] {
                b'.' if !has_dot => {
                    has_dot = true;
                    self.pos += 1;
                }
                b'0'..=b'9' => self.pos += 1,
                _ => break,
            }
        }
        let s = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| PdfError::parse(start, "invalid number"))?;
        if has_dot {
            Ok(Token::Real(
                s.parse()
                    .map_err(|_| PdfError::parse(start, "invalid real"))?,
            ))
        } else {
            let digits = s.trim_start_matches(['+', '-']);
            if digits.is_empty() {
                return Err(PdfError::parse(start, "invalid integer"));
            }
            // Only digits remain, so a parse failure means overflow. Valid
            // PDFs never need such values, but some writers emit garbage
            // ones (Apple's writes 20-digit object numbers in references).
            // Saturate rather than reject the whole object around it.
            Ok(Token::Integer(s.parse().unwrap_or(if s.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            })))
        }
    }

    fn keyword(&mut self) -> Result<Token> {
        let start = self.pos;
        while self.pos < self.data.len()
            && !is_delimiter(self.data[self.pos])
            && !is_ws(self.data[self.pos])
        {
            self.pos += 1;
        }
        let word = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| PdfError::parse(start, "invalid keyword"))?;
        Ok(match word {
            "null" => Token::Null,
            "true" => Token::Bool(true),
            "false" => Token::Bool(false),
            _ => Token::Keyword(Keyword::from_word(word)),
        })
    }
}

/// A bare word: an operator in a content stream, or `obj`, `R` and the like
/// in the file's structure. The words PDF defines are held as a small index
/// rather than a string — a drawing's content stream is hundreds of thousands
/// of them — which a renderer can also store and look up directly.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Keyword(Word);

#[derive(Clone, PartialEq, Eq, Hash)]
enum Word {
    Known(u8),
    Other(String),
}

/// The content stream operators (ISO 32000-1, Annex A) and the file
/// structure's keywords. [`Keyword::known_index`] is a position here.
pub const KNOWN_KEYWORDS: [&str; 81] = [
    "b",
    "B",
    "b*",
    "B*",
    "BDC",
    "BI",
    "BMC",
    "BT",
    "BX",
    "c",
    "cm",
    "CS",
    "cs",
    "d",
    "d0",
    "d1",
    "Do",
    "DP",
    "EI",
    "EMC",
    "ET",
    "EX",
    "f",
    "F",
    "f*",
    "G",
    "g",
    "gs",
    "h",
    "i",
    "ID",
    "j",
    "J",
    "K",
    "k",
    "l",
    "m",
    "M",
    "MP",
    "n",
    "q",
    "Q",
    "re",
    "RG",
    "rg",
    "ri",
    "s",
    "S",
    "SC",
    "sc",
    "SCN",
    "scn",
    "sh",
    "T*",
    "Tc",
    "Td",
    "TD",
    "Tf",
    "Tj",
    "TJ",
    "TL",
    "Tm",
    "Tr",
    "Ts",
    "Tw",
    "Tz",
    "v",
    "w",
    "W",
    "W*",
    "y",
    "'",
    "\"",
    "obj",
    "endobj",
    "stream",
    "endstream",
    "R",
    "xref",
    "trailer",
    "startxref",
];

/// The index of `word` in [`KNOWN_KEYWORDS`]: candidates are found by first
/// byte, so a lookup compares against a handful of words, not all of them.
fn known_index(word: &[u8]) -> Option<u8> {
    static BY_FIRST_BYTE: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    let table = BY_FIRST_BYTE.get_or_init(|| {
        let mut table = vec![Vec::new(); 256];
        for (index, known) in KNOWN_KEYWORDS.iter().enumerate() {
            table[usize::from(known.as_bytes()[0])].push(index as u8);
        }
        table
    });
    table[usize::from(*word.first()?)]
        .iter()
        .copied()
        .find(|&index| KNOWN_KEYWORDS[usize::from(index)].as_bytes() == word)
}

impl Keyword {
    pub fn from_word(word: &str) -> Self {
        Self(match known_index(word.as_bytes()) {
            Some(index) => Word::Known(index),
            None => Word::Other(word.to_owned()),
        })
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Word::Known(index) => KNOWN_KEYWORDS[usize::from(*index)],
            Word::Other(word) => word,
        }
    }

    /// The keyword's position in [`KNOWN_KEYWORDS`], when PDF defines it.
    pub fn known_index(&self) -> Option<u8> {
        match self.0 {
            Word::Known(index) => Some(index),
            Word::Other(_) => None,
        }
    }
}

impl std::ops::Deref for Keyword {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Keyword {
    fn from(word: &str) -> Self {
        Self::from_word(word)
    }
}

impl From<String> for Keyword {
    fn from(word: String) -> Self {
        match known_index(word.as_bytes()) {
            Some(index) => Self(Word::Known(index)),
            None => Self(Word::Other(word)),
        }
    }
}

impl PartialEq<str> for Keyword {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Keyword {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Debug for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn is_ws(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

pub fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturates_integers_too_large_for_64_bits() {
        let mut lexer = Lexer::new(b"18446744073386459286 -99999999999999999999 +5");
        let mut next = || lexer.next_token().unwrap().unwrap().token;
        assert_eq!(next(), Token::Integer(i64::MAX));
        assert_eq!(next(), Token::Integer(i64::MIN));
        assert_eq!(next(), Token::Integer(5));
        // A sign with no digits is still not a number.
        assert!(Lexer::new(b"- ").next_token().is_err());
    }

    #[test]
    fn lexes_basic_tokens() {
        let mut lexer = Lexer::new(b"/Type /Page [null true -7 3.5 (hi\\n) <4869>]");
        let mut tokens = Vec::new();
        while let Some(tok) = lexer.next_token().unwrap() {
            tokens.push(tok.token);
        }
        assert_eq!(tokens[0], Token::Name("Type".into()));
        assert_eq!(tokens[2], Token::ArrayStart);
        assert_eq!(tokens[5], Token::Integer(-7));
        assert_eq!(tokens[6], Token::Real(3.5));
        assert_eq!(tokens[7], Token::LiteralString(b"hi\n".to_vec()));
        assert_eq!(tokens[8], Token::HexString(b"Hi".to_vec()));
    }
}
