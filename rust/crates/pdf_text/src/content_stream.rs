//! Milestone 8 (part 1): content stream operator parsing.

use pdf_core::error::{PdfError, Result};
use pdf_core::lexer::{is_ws, Lexer, Token};
use pdf_core::object::{Dictionary, PdfObject};

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub operator: String,
    pub operands: Vec<PdfObject>,
}

/// Parse a (decoded) content stream into a flat list of operations.
/// Inline images (`BI … ID <binary> EI`) are skipped.
pub fn parse_content(data: &[u8]) -> Result<Vec<Operation>> {
    let mut lexer = Lexer::new(data);
    let mut operations = Vec::new();
    let mut operands: Vec<PdfObject> = Vec::new();
    // Stack of partially-built containers: arrays and dictionaries.
    enum Frame {
        Array(Vec<PdfObject>),
        Dict(Dictionary, Option<String>),
    }
    let mut stack: Vec<Frame> = Vec::new();

    fn push_value(
        stack: &mut Vec<Frame>,
        operands: &mut Vec<PdfObject>,
        value: PdfObject,
    ) -> Result<()> {
        match stack.last_mut() {
            Some(Frame::Array(items)) => items.push(value),
            Some(Frame::Dict(dict, pending_key)) => match pending_key.take() {
                Some(key) => {
                    dict.insert(key, value);
                }
                None => match value {
                    PdfObject::Name(key) => *pending_key = Some(key),
                    _ => {
                        return Err(PdfError::Structure(
                            "dictionary key must be a name".into(),
                        ))
                    }
                },
            },
            None => operands.push(value),
        }
        Ok(())
    }

    while let Some(spanned) = lexer.next_token()? {
        match spanned.token {
            Token::Null => push_value(&mut stack, &mut operands, PdfObject::Null)?,
            Token::Bool(v) => push_value(&mut stack, &mut operands, PdfObject::Bool(v))?,
            Token::Integer(v) => push_value(&mut stack, &mut operands, PdfObject::Integer(v))?,
            Token::Real(v) => push_value(&mut stack, &mut operands, PdfObject::Real(v))?,
            Token::Name(v) => {
                // Inside a dict body, names may be keys; push_value handles it.
                push_value(&mut stack, &mut operands, PdfObject::Name(v))?
            }
            Token::LiteralString(v) => {
                push_value(&mut stack, &mut operands, PdfObject::LiteralString(v))?
            }
            Token::HexString(v) => {
                push_value(&mut stack, &mut operands, PdfObject::HexString(v))?
            }
            Token::ArrayStart => stack.push(Frame::Array(Vec::new())),
            Token::ArrayEnd => match stack.pop() {
                Some(Frame::Array(items)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Array(items))?
                }
                _ => return Err(PdfError::Structure("unbalanced ] in content".into())),
            },
            Token::DictStart => stack.push(Frame::Dict(Dictionary::new(), None)),
            Token::DictEnd => match stack.pop() {
                Some(Frame::Dict(dict, _)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Dictionary(dict))?
                }
                _ => return Err(PdfError::Structure("unbalanced >> in content".into())),
            },
            Token::Keyword(word) => {
                if !stack.is_empty() {
                    return Err(PdfError::Structure(format!(
                        "operator {word} inside an unterminated container"
                    )));
                }
                if word == "BI" {
                    skip_inline_image(&mut lexer, data)?;
                    operands.clear();
                    continue;
                }
                operations.push(Operation {
                    operator: word,
                    operands: std::mem::take(&mut operands),
                });
            }
        }
    }
    Ok(operations)
}

/// After a `BI`, scan forward past `ID <binary data> EI`.
fn skip_inline_image(lexer: &mut Lexer, data: &[u8]) -> Result<()> {
    // Find the ID keyword by scanning raw bytes from the current position.
    let mut pos = lexer.position();
    while pos + 1 < data.len() {
        if data[pos] == b'I' && data[pos + 1] == b'D' {
            pos += 2;
            // One whitespace byte follows ID before the binary data.
            if pos < data.len() && is_ws(data[pos]) {
                pos += 1;
            }
            break;
        }
        pos += 1;
    }
    // Scan for EI delimited by whitespace.
    while pos + 1 < data.len() {
        if data[pos] == b'E'
            && data[pos + 1] == b'I'
            && (pos == 0 || is_ws(data[pos - 1]))
            && (pos + 2 >= data.len() || is_ws(data[pos + 2]) || pos + 2 == data.len())
        {
            lexer.set_position(pos + 2);
            return Ok(());
        }
        pos += 1;
    }
    lexer.set_position(data.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_operations() {
        let content = b"BT /F1 12 Tf 72 720 Td (Hello) Tj [ (W) -120 (orld) ] TJ ET";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BT", "Tf", "Td", "Tj", "TJ", "ET"]);
        assert_eq!(ops[1].operands[0], PdfObject::Name("F1".into()));
        assert_eq!(ops[3].operands[0], PdfObject::LiteralString(b"Hello".to_vec()));
        match &ops[4].operands[0] {
            PdfObject::Array(items) => assert_eq!(items.len(), 3),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn parses_dict_operand_and_skips_inline_images() {
        let content =
            b"/GS1 gs BDC BI /W 2 /H 2 /CS /G /BPC 8 ID \x00\x01\x02\x03 EI Q (after) Tj";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert!(names.contains(&"Tj"), "operators after inline image survive");
        assert!(!names.contains(&"EI"), "inline image is skipped: {names:?}");
    }

    #[test]
    fn marked_content_dictionaries() {
        let content = b"/Span << /ActualText (hi) >> BDC ET";
        let ops = parse_content(content).unwrap();
        assert_eq!(ops[0].operator, "BDC");
        assert_eq!(ops[0].operands.len(), 2);
        assert!(matches!(ops[0].operands[1], PdfObject::Dictionary(_)));
    }
}
