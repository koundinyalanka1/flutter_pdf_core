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
    Keyword(String),
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
        let marker = b"endstream";
        let marker_offset = self.data[start..]
            .windows(marker.len())
            .position(|window| window == marker)
            .map(|relative| start + relative)
            .ok_or_else(|| PdfError::parse(stream_keyword_offset, "unterminated stream"))?;

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
            Ok(Token::Integer(
                s.parse()
                    .map_err(|_| PdfError::parse(start, "invalid integer"))?,
            ))
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
            _ => Token::Keyword(word.to_owned()),
        })
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
