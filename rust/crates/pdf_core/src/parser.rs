use crate::error::{PdfError, Result};
use crate::lexer::{Lexer, SpannedToken, Token};
use crate::object::{Dictionary, IndirectObject, ObjectId, PdfObject};
use crate::stream::PdfStream;

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    lookahead: Vec<SpannedToken>,
    depth: usize,
}

// PDF files are untrusted input. A stack overflow aborts the process even
// when the FFI entry point catches ordinary Rust panics.
const MAX_OBJECT_DEPTH: usize = 128;

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            lexer: Lexer::new(data),
            lookahead: Vec::new(),
            depth: 0,
        }
    }

    pub fn with_offset(data: &'a [u8], offset: usize) -> Self {
        Self {
            lexer: Lexer::with_offset(data, offset),
            lookahead: Vec::new(),
            depth: 0,
        }
    }

    pub fn position(&self) -> usize {
        self.lookahead
            .first()
            .map(|t| t.offset)
            .unwrap_or_else(|| self.lexer.position())
    }

    pub fn parse_object(&mut self) -> Result<PdfObject> {
        if self.depth >= MAX_OBJECT_DEPTH {
            return Err(PdfError::parse(self.position(), "object nesting limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_object_inner();
        self.depth -= 1;
        result
    }

    fn parse_object_inner(&mut self) -> Result<PdfObject> {
        self.fill(3)?;
        if let [SpannedToken {
            token: Token::Integer(obj),
            ..
        }, SpannedToken {
            token: Token::Integer(gen),
            ..
        }, SpannedToken {
            token: Token::Keyword(r),
            ..
        }, ..] = self.lookahead.as_slice()
        {
            if r == "R" {
                // Numbers outside the object-id range cannot name any object.
                // A reference to a missing object is null in PDF, so read it
                // as null: never wrapped onto a real object, and never fatal
                // to the object that contains it.
                let id = checked_object_id(*obj, *gen, self.position()).ok();
                self.take()?;
                self.take()?;
                self.take()?;
                return Ok(id.map_or(PdfObject::Null, PdfObject::Reference));
            }
        }

        let tok = self
            .take()?
            .ok_or_else(|| PdfError::parse(self.position(), "expected object"))?;
        match tok.token {
            Token::Null => Ok(PdfObject::Null),
            Token::Bool(v) => Ok(PdfObject::Bool(v)),
            Token::Integer(v) => Ok(PdfObject::Integer(v)),
            Token::Real(v) => Ok(PdfObject::Real(v)),
            Token::Name(v) => Ok(PdfObject::Name(v)),
            Token::LiteralString(v) => Ok(PdfObject::LiteralString(v)),
            Token::HexString(v) => Ok(PdfObject::HexString(v)),
            Token::ArrayStart => self.parse_array(tok.offset),
            Token::DictStart => self.parse_dictionary(tok.offset),
            other => Err(PdfError::parse(
                tok.offset,
                format!("unexpected token {other:?}"),
            )),
        }
    }

    pub fn parse_indirect_object(&mut self) -> Result<IndirectObject> {
        let obj = self.expect_integer("object number")?;
        let gen = self.expect_integer("generation number")?;
        let id = checked_object_id(obj, gen, self.position())?;
        self.expect_keyword("obj")?;
        let mut value = self.parse_object()?;
        self.fill(1)?;
        if matches!(
            self.lookahead.first().map(|token| &token.token),
            Some(Token::Keyword(keyword)) if keyword == "stream"
        ) {
            let stream_token = self.take()?.expect("lookahead was just filled");
            let dictionary = match value {
                PdfObject::Dictionary(dictionary) => dictionary,
                _ => {
                    return Err(PdfError::parse(
                        stream_token.offset,
                        "stream object must have a dictionary",
                    ));
                }
            };
            let length_hint = dictionary
                .get("Length")
                .and_then(PdfObject::as_i64)
                .and_then(|v| usize::try_from(v).ok());
            let data = self
                .lexer
                .read_stream_data_with_length(stream_token.offset, length_hint)?;
            value = PdfObject::Stream(PdfStream::new(dictionary, data));
        }
        self.fill(1)?;
        if matches!(
            self.lookahead.first().map(|token| &token.token),
            Some(Token::Keyword(keyword)) if keyword == "endobj"
        ) {
            self.take()?;
        } else if !self.at_structural_boundary() {
            return Err(PdfError::parse(self.position(), "expected endobj"));
        }
        Ok(IndirectObject { id, value })
    }

    /// `endobj` is required, but some writers omit it (Apple's does after
    /// `endstream`). An object followed directly by the next object, the
    /// index or the end of the file is complete, so it is accepted as other
    /// readers accept it. Anything else after it, such as stream bytes past a
    /// wrong `/Length`, still means the object did not end where it seemed to.
    fn at_structural_boundary(&mut self) -> bool {
        if self.fill(3).is_err() {
            return false;
        }
        match self.lookahead.as_slice() {
            [] => true,
            [SpannedToken {
                token: Token::Keyword(keyword),
                ..
            }, ..] => matches!(keyword.as_str(), "xref" | "trailer" | "startxref"),
            [SpannedToken {
                token: Token::Integer(_),
                ..
            }, SpannedToken {
                token: Token::Integer(_),
                ..
            }, SpannedToken {
                token: Token::Keyword(keyword),
                ..
            }, ..] => keyword == "obj",
            _ => false,
        }
    }

    fn parse_array(&mut self, start: usize) -> Result<PdfObject> {
        let mut items = Vec::new();
        loop {
            self.fill(1)?;
            match self.lookahead.first().map(|t| &t.token) {
                Some(Token::ArrayEnd) => {
                    self.take()?;
                    return Ok(PdfObject::Array(items));
                }
                Some(_) => items.push(self.parse_object()?),
                None => return Err(PdfError::parse(start, "unterminated array")),
            }
        }
    }

    fn parse_dictionary(&mut self, start: usize) -> Result<PdfObject> {
        let mut dict = Dictionary::new();
        loop {
            self.fill(1)?;
            match self.lookahead.first().map(|t| &t.token) {
                Some(Token::DictEnd) => {
                    self.take()?;
                    return Ok(PdfObject::Dictionary(dict));
                }
                Some(Token::Name(_)) => {
                    let key = match self.take()?.unwrap().token {
                        Token::Name(key) => key,
                        _ => unreachable!(),
                    };
                    let value = self.parse_object()?;
                    dict.insert(key, value);
                }
                Some(_) => return Err(PdfError::parse(self.position(), "expected dictionary key")),
                None => return Err(PdfError::parse(start, "unterminated dictionary")),
            }
        }
    }

    fn expect_integer(&mut self, what: &str) -> Result<i64> {
        let tok = self
            .take()?
            .ok_or_else(|| PdfError::parse(self.position(), format!("expected {what}")))?;
        match tok.token {
            Token::Integer(v) => Ok(v),
            _ => Err(PdfError::parse(tok.offset, format!("expected {what}"))),
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        let tok = self
            .take()?
            .ok_or_else(|| PdfError::parse(self.position(), format!("expected {keyword}")))?;
        match tok.token {
            Token::Keyword(actual) if actual == keyword => Ok(()),
            _ => Err(PdfError::parse(tok.offset, format!("expected {keyword}"))),
        }
    }

    fn fill(&mut self, n: usize) -> Result<()> {
        while self.lookahead.len() < n {
            match self.lexer.next_token()? {
                Some(tok) => self.lookahead.push(tok),
                None => break,
            }
        }
        Ok(())
    }

    fn take(&mut self) -> Result<Option<SpannedToken>> {
        self.fill(1)?;
        if self.lookahead.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.lookahead.remove(0)))
        }
    }
}

fn checked_object_id(number: i64, generation: i64, offset: usize) -> Result<ObjectId> {
    let number = u32::try_from(number)
        .map_err(|_| PdfError::parse(offset, "object number is out of range"))?;
    let generation = u16::try_from(generation)
        .map_err(|_| PdfError::parse(offset, "generation number is out of range"))?;
    Ok(ObjectId::new(number, generation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_deeply_nested_objects_without_overflowing_the_stack() {
        for (open, close) in [("[", "]"), ("<< /Nested ", ">>")] {
            let input = format!("{}null{}", open.repeat(1000), close.repeat(1000));
            let err = Parser::new(input.as_bytes()).parse_object().unwrap_err();
            assert!(err.to_string().contains("nesting limit"));
        }
    }

    #[test]
    fn rejects_object_ids_that_would_wrap() {
        for (number, generation) in [(4_294_967_297i64, 0), (1, 65_536), (-1, 0), (1, -1)] {
            // A reference can never wrap onto a real object; it names nothing,
            // which PDF reads as null.
            let reference = format!("{number} {generation} R");
            assert_eq!(
                Parser::new(reference.as_bytes()).parse_object().unwrap(),
                PdfObject::Null,
                "{reference}"
            );
            let indirect = format!("{number} {generation} obj null endobj");
            assert!(
                Parser::new(indirect.as_bytes())
                    .parse_indirect_object()
                    .is_err(),
                "{indirect}"
            );
        }
    }

    #[test]
    fn accepts_objects_whose_writer_left_out_endobj() {
        let mut parser =
            Parser::new(b"7 0 obj\n<< /Length 3 >>\nstream\nabc\nendstream\n8 0 obj null endobj");
        let stream = parser.parse_indirect_object().unwrap();
        assert_eq!(stream.id, ObjectId::new(7, 0));
        assert!(matches!(stream.value, PdfObject::Stream(_)));
        // Parsing carries on with the object that follows.
        assert_eq!(parser.parse_indirect_object().unwrap().id, ObjectId::new(8, 0));

        let last = Parser::new(b"9 0 obj (text)").parse_indirect_object().unwrap();
        assert_eq!(last.id, ObjectId::new(9, 0));
        let before_index = Parser::new(b"10 0 obj [1 2]\nxref\n0 1")
            .parse_indirect_object()
            .unwrap();
        assert_eq!(before_index.id, ObjectId::new(10, 0));
    }

    #[test]
    fn still_rejects_objects_followed_by_stray_data() {
        // Stream bytes past a wrong /Length are not a clean end of object.
        for input in [
            &b"7 0 obj\n<< /Length 3 >>\nstream\nabc\nendstream\nq 1 0 0 1 cm"[..],
            &b"8 0 obj (text) 42 /Name"[..],
        ] {
            let error = Parser::new(input).parse_indirect_object().unwrap_err();
            assert!(error.to_string().contains("endobj"), "{error}");
        }
    }

    #[test]
    fn preserves_object_id_boundaries() {
        let id = ObjectId::new(u32::MAX, u16::MAX);
        assert_eq!(
            Parser::new(b"4294967295 65535 R").parse_object().unwrap(),
            PdfObject::Reference(id)
        );
        assert_eq!(
            Parser::new(b"4294967295 65535 obj null endobj")
                .parse_indirect_object()
                .unwrap()
                .id,
            id
        );
    }

    #[test]
    fn parses_primitives_and_reference() {
        let mut parser = Parser::new(b"<< /Type /Page /Parent 2 0 R /Nums [1 2.5 false] >>");
        let obj = parser.parse_object().unwrap();
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict["Type"].as_name(), Some("Page"));
        assert_eq!(dict["Parent"].as_ref(), Some(ObjectId::new(2, 0)));
    }

    #[test]
    fn parses_indirect_object() {
        let mut parser = Parser::new(b"1 0 obj << /Type /Catalog >> endobj");
        let obj = parser.parse_indirect_object().unwrap();
        assert_eq!(obj.id, ObjectId::new(1, 0));
    }

    #[test]
    fn parses_stream_object() {
        let mut parser = Parser::new(
            b"4 0 obj
<< /Length 5 >>
stream
hello
endstream
endobj",
        );
        let obj = parser.parse_indirect_object().unwrap();
        match obj.value {
            PdfObject::Stream(stream) => {
                assert_eq!(stream.dictionary["Length"].as_i64(), Some(5));
                assert_eq!(stream.data, b"hello");
            }
            other => panic!("expected stream, got {other:?}"),
        }
    }
}
