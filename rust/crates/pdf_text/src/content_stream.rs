//! Milestone 8 (part 1): content stream operator parsing.

use pdf_core::error::Result;
use pdf_core::lexer::{is_delimiter, is_ws, Lexer, Token};
use pdf_core::object::{Dictionary, PdfObject};

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub operator: String,
    pub operands: Vec<PdfObject>,
}

/// Parse a (decoded) content stream into a flat list of operations.
/// Inline images become `BI` operations containing a stream operand. A damaged
/// inline image has no operand, allowing renderers to report the omission.
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
            Token::HexString(v) => push_value(&mut stack, &mut operands, PdfObject::HexString(v)),
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
                    let image = read_inline_image(&mut lexer, data);
                    operations.push(Operation {
                        operator: "BI".into(),
                        operands: image.into_iter().map(PdfObject::Stream).collect(),
                    });
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

/// Parse the small inline dictionary before touching binary bytes. For raw
/// images the exact row length prevents an embedded " EI " pixel sequence from
/// terminating the image. Filtered payloads use the PDF delimiter convention.
fn read_inline_image(lexer: &mut Lexer, data: &[u8]) -> Option<pdf_core::stream::PdfStream> {
    let header_start = lexer.position();
    let header_end = loop {
        match lexer.next_token() {
            Ok(Some(token)) if token.token == Token::Keyword("ID".into()) => break token.offset,
            Ok(Some(_)) => {}
            _ => {
                lexer.set_position(data.len());
                return None;
            }
        }
    };
    let mut header = b"<< ".to_vec();
    header.extend_from_slice(&data[header_start..header_end]);
    header.extend_from_slice(b" >>");
    let dict = pdf_core::parser::Parser::new(&header)
        .parse_object()
        .ok()
        .and_then(|object| object.as_dict().cloned());
    let mut start = lexer.position();
    if data.get(start) == Some(&b'\r') && data.get(start + 1) == Some(&b'\n') {
        start += 2;
    } else if data.get(start).copied().map(is_ws).unwrap_or(false) {
        start += 1;
    }
    let raw_len = dict
        .as_ref()
        .filter(|d| !d.contains_key("F") && !d.contains_key("Filter"))
        .and_then(inline_sample_length);
    let mut candidates = 0;
    let mut pos = raw_len.and_then(|n| start.checked_add(n)).unwrap_or(start);
    while pos + 1 < data.len() {
        if data[pos] == b'E'
            && data[pos + 1] == b'I'
            && pos > start
            && is_ws(data[pos - 1])
            && (pos + 2 == data.len() || is_ws(data[pos + 2]) || is_delimiter(data[pos + 2]))
        {
            let end = raw_len
                .and_then(|n| start.checked_add(n))
                .unwrap_or(pos - 1);
            candidates += 1;
            if raw_len.is_none()
                && candidates <= 8
                && dict
                    .as_ref()
                    .is_some_and(|d| !inline_payload_complete(d, &data[start..end]))
            {
                pos += 2;
                continue;
            }
            lexer.set_position(pos + 2);
            if candidates > 8 {
                return None;
            }
            if end > pos || (raw_len.is_some() && !data[end..pos].iter().all(|b| is_ws(*b))) {
                return None;
            }
            return dict.map(|d| pdf_core::stream::PdfStream::new(d, data[start..end].to_vec()));
        }
        pos += 1;
    }
    lexer.set_position(data.len());
    None
}

/// EI can occur inside compressed bytes too. For the byte-oriented filters
/// which the core decodes, validate candidate delimiters against the expected
/// sample count; truncated deflate is intentionally tolerated elsewhere, but
/// must not truncate an inline image before all of its pixels are present.
/// Bound retries and sample count so this recovery cannot amplify untrusted
/// compressed input without limit. Image codecs retain delimiter scanning.
fn inline_payload_complete(dict: &Dictionary, bytes: &[u8]) -> bool {
    let Some(expected) = inline_sample_length(dict).filter(|n| *n <= 8_000_000) else {
        return true;
    };
    let filter = dict.get("F").or_else(|| dict.get("Filter"));
    let names: Vec<&str> = match filter {
        Some(PdfObject::Name(n)) => vec![n],
        Some(PdfObject::Array(items)) => items.iter().filter_map(PdfObject::as_name).collect(),
        _ => return true,
    };
    if names.is_empty()
        || names.iter().any(|n| {
            !matches!(
                *n,
                "Fl" | "FlateDecode"
                    | "LZW"
                    | "LZWDecode"
                    | "AHx"
                    | "ASCIIHexDecode"
                    | "A85"
                    | "ASCII85Decode"
                    | "RL"
                    | "RunLengthDecode"
            )
        })
    {
        return true;
    }
    let mut normalized = dict.clone();
    if let Some(filter) = normalized.remove("F") {
        normalized.insert("Filter".into(), filter);
    }
    pdf_core::filter::decode_with_dict(&normalized, bytes)
        .is_ok_and(|decoded| decoded.len() >= expected)
}

fn inline_sample_length(dict: &Dictionary) -> Option<usize> {
    let number = |short: &str, long: &str| {
        dict.get(short)
            .or_else(|| dict.get(long))
            .and_then(PdfObject::as_i64)
            .and_then(|n| usize::try_from(n).ok())
    };
    let width = number("W", "Width")?;
    let height = number("H", "Height")?;
    let mask = matches!(
        dict.get("IM").or_else(|| dict.get("ImageMask")),
        Some(PdfObject::Bool(true))
    );
    let bits = if mask {
        1
    } else {
        number("BPC", "BitsPerComponent")?
    };
    let components = if mask {
        1
    } else {
        match dict
            .get("CS")
            .or_else(|| dict.get("ColorSpace"))
            .and_then(PdfObject::as_name)?
        {
            "G" | "DeviceGray" => 1,
            "RGB" | "DeviceRGB" => 3,
            "CMYK" | "DeviceCMYK" => 4,
            _ => return None,
        }
    };
    width
        .checked_mul(components)?
        .checked_mul(bits)?
        .checked_add(7)?
        .checked_div(8)?
        .checked_mul(height)
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
        assert_eq!(
            ops[3].operands[0],
            PdfObject::LiteralString(b"Hello".to_vec())
        );
        match &ops[4].operands[0] {
            PdfObject::Array(items) => assert_eq!(items.len(), 3),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn parses_inline_images_as_stream_operands() {
        let content = b"/GS1 gs BDC BI /W 2 /H 2 /CS /G /BPC 8 ID \x00\x01\x02\x03 EI Q (after) Tj";
        let ops = parse_content(content).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert!(
            names.contains(&"Tj"),
            "operators after inline image survive"
        );
        assert!(
            !names.contains(&"EI"),
            "inline data is one BI operation: {names:?}"
        );
        let image = ops.iter().find(|op| op.operator == "BI").unwrap();
        assert!(
            matches!(image.operands.first(), Some(PdfObject::Stream(s)) if s.data == [0,1,2,3])
        );
    }

    #[test]
    fn inline_raw_ei_pixel_bytes_do_not_terminate_the_image() {
        let data = b"BI /W 5 /H 1 /CS /G /BPC 8 ID \x00 EI  EI/Q gs";
        let ops = parse_content(data).unwrap();
        assert!(matches!(&ops[0].operands[0], PdfObject::Stream(s) if s.data == b"\x00 EI "));
        assert_eq!(ops[1].operator, "gs");
    }

    #[test]
    fn inline_compressed_ei_bytes_are_checked_against_sample_count() {
        // Zlib with an uncompressed DEFLATE block and a literal EI sequence.
        let raw = b"abc EI xyz";
        let len = raw.len() as u16;
        let mut compressed = vec![0x78, 0x01, 0x01];
        compressed.extend_from_slice(&len.to_le_bytes());
        compressed.extend_from_slice(&(!len).to_le_bytes());
        compressed.extend_from_slice(raw);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in raw {
            a = (a + *byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        compressed.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut data = format!("BI /W {} /H 1 /CS /G /BPC 8 /F /Fl ID ", raw.len()).into_bytes();
        data.extend_from_slice(&compressed);
        data.extend_from_slice(b" EI Q (after) Tj");
        let ops = parse_content(&data).unwrap();
        assert!(matches!(&ops[0].operands[0], PdfObject::Stream(s) if s.data == compressed));
        assert_eq!(ops.last().unwrap().operator, "Tj");
    }

    #[test]
    fn inline_short_packed_rows_keep_byte_padding() {
        let ops = parse_content(b"BI /W 3 /H 2 /IM true ID \x80\x40 EI Q").unwrap();
        assert!(matches!(&ops[0].operands[0], PdfObject::Stream(s) if s.data == [0x80,0x40]));
        assert_eq!(ops[1].operator, "Q");
    }

    #[test]
    fn truncated_inline_image_is_an_explicit_empty_bi_operation() {
        let ops = parse_content(b"q BI /W 20 /H 20 /CS /G /BPC 8 ID short").unwrap();
        assert_eq!(ops[1].operator, "BI");
        assert!(ops[1].operands.is_empty());
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
