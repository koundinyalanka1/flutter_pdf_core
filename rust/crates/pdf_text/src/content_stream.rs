//! Milestone 8 (part 1): content stream operator parsing.

use pdf_core::error::Result;
use pdf_core::lexer::{is_ws, Lexer, Token};
use pdf_core::object::{Dictionary, PdfObject};

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub operator: String,
    pub operands: Vec<PdfObject>,
}

/// Parse a (decoded) content stream into a flat list of operations.
/// Inline images (`BI … ID <binary> EI`) are skipped.
///
/// Damaged content is recovered from rather than rejected. Real files carry
/// stray delimiters, unbalanced containers and truncated tokens, and the
/// operators around the damage are usually perfectly good — so a bad token
/// costs the reader that token, not the page. The function therefore always
/// succeeds; it returns whatever it could make sense of.
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

    /// A container nested deeper than this is damage, not intent; ignoring
    /// the opener keeps a malformed stream from growing the stack forever.
    const MAX_DEPTH: usize = 64;

    fn push_value(stack: &mut [Frame], operands: &mut Vec<PdfObject>, value: PdfObject) {
        match stack.last_mut() {
            Some(Frame::Array(items)) => items.push(value),
            Some(Frame::Dict(dict, pending_key)) => match pending_key.take() {
                Some(key) => {
                    dict.insert(key, value);
                }
                None => match value {
                    PdfObject::Name(key) => *pending_key = Some(key),
                    // A non-name where a key belongs: drop it and keep going.
                    _ => {}
                },
            },
            None => operands.push(value),
        }
    }

    /// Collapse every open container into its parent, innermost first, so a
    /// stream that never closed a `[` still yields the operator that follows.
    fn unwind(stack: &mut Vec<Frame>, operands: &mut Vec<PdfObject>) {
        while let Some(frame) = stack.pop() {
            let value = match frame {
                Frame::Array(items) => PdfObject::Array(items),
                Frame::Dict(dict, _) => PdfObject::Dictionary(dict),
            };
            push_value(stack, operands, value);
        }
    }

    loop {
        // Remember where this token began: a lexer error must still advance,
        // or recovery would spin on the same byte.
        let began = lexer.position();
        let spanned = match lexer.next_token() {
            Ok(Some(spanned)) => spanned,
            Ok(None) => break,
            Err(_) => {
                if began + 1 >= data.len() {
                    break;
                }
                lexer.set_position(began + 1);
                continue;
            }
        };

        match spanned.token {
            Token::Null => push_value(&mut stack, &mut operands, PdfObject::Null),
            Token::Bool(v) => push_value(&mut stack, &mut operands, PdfObject::Bool(v)),
            Token::Integer(v) => push_value(&mut stack, &mut operands, PdfObject::Integer(v)),
            Token::Real(v) => push_value(&mut stack, &mut operands, PdfObject::Real(v)),
            Token::Name(v) => {
                // Inside a dict body, names may be keys; push_value handles it.
                push_value(&mut stack, &mut operands, PdfObject::Name(v))
            }
            Token::LiteralString(v) => {
                push_value(&mut stack, &mut operands, PdfObject::LiteralString(v))
            }
            Token::HexString(v) => {
                push_value(&mut stack, &mut operands, PdfObject::HexString(v))
            }
            Token::ArrayStart => {
                if stack.len() < MAX_DEPTH {
                    stack.push(Frame::Array(Vec::new()));
                }
            }
            Token::DictStart => {
                if stack.len() < MAX_DEPTH {
                    stack.push(Frame::Dict(Dictionary::new(), None));
                }
            }
            // A closer with nothing open, or closing the wrong kind of
            // container, is dropped: the content before it is still usable.
            Token::ArrayEnd => match stack.pop() {
                Some(Frame::Array(items)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Array(items))
                }
                Some(Frame::Dict(dict, _)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Dictionary(dict))
                }
                None => {}
            },
            Token::DictEnd => match stack.pop() {
                Some(Frame::Dict(dict, _)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Dictionary(dict))
                }
                Some(Frame::Array(items)) => {
                    push_value(&mut stack, &mut operands, PdfObject::Array(items))
                }
                None => {}
            },
            Token::Keyword(word) => {
                // An operator ends any container still open above it.
                if !stack.is_empty() {
                    unwind(&mut stack, &mut operands);
                }
                if word == "BI" {
                    let _ = skip_inline_image(&mut lexer, data);
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
    fn stray_closer_does_not_discard_the_page() {
        // The regression this guards: one loose `>>` used to abort the whole
        // parse, so everything before it was lost as well.
        let content = b"0 0 1 rg 72 600 300 100 re f >> 1 0 0 rg 72 400 300 100 re f";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["rg", "re", "f", "rg", "re", "f"]);
    }

    #[test]
    fn unterminated_array_still_yields_following_operators() {
        let content = b"BT [ (a) -20 (b) Tj ET 1 0 0 rg";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BT", "Tj", "ET", "rg"]);
    }

    #[test]
    fn unterminated_dictionary_is_closed_at_the_next_operator() {
        let content = b"<< /Type /Foo BDC (after) Tj";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BDC", "Tj"]);
    }

    #[test]
    fn stray_array_closer_is_ignored() {
        let content = b"72 700 Td ] (text) Tj";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["Td", "Tj"]);
    }

    #[test]
    fn damaged_token_costs_only_that_token() {
        // An unterminated literal string runs to the end of the data; the
        // operators before it must survive.
        let content = b"1 0 0 rg 10 10 50 50 re f (unterminated";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert!(names.starts_with(&["rg", "re", "f"]), "got {names:?}");
    }

    #[test]
    fn deeply_nested_garbage_terminates() {
        let content = [b"[".repeat(5000), b" 1 2 Td".to_vec()].concat();
        let ops = parse_content(&content).unwrap();
        assert_eq!(ops.last().map(|o| o.operator.as_str()), Some("Td"));
    }

    #[test]
    fn non_name_dictionary_key_is_dropped_not_fatal() {
        let content = b"<< 42 /Value /Type /Good >> BDC (x) Tj";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BDC", "Tj"]);
    }

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
