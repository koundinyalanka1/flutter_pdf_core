//! CCITT Group 3/4 fax decoding (`/CCITTFaxDecode`).
//!
//! This is what scanners and fax-derived PDFs put their page images in, so
//! without it a large share of scanned documents render as blank white pages.
//!
//! Implements ITU-T T.4 (Group 3, one- and two-dimensional) and T.6 (Group 4,
//! purely two-dimensional), selected by `/K` exactly as the PDF specification
//! describes: negative is G4, zero is G3 1-D, positive is G3 mixed.
//!
//! Decoding is deliberately forgiving. Fax data is often truncated or
//! slightly malformed, and half a scanned page is worth far more to a reader
//! than an error, so a row that will not decode ends the image and everything
//! above it is kept.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `/DecodeParms` for a CCITT stream.
#[derive(Debug, Clone, Copy)]
pub struct CcittParams {
    /// < 0: Group 4. 0: Group 3 one-dimensional. > 0: Group 3 mixed.
    pub k: i32,
    pub columns: usize,
    /// 0 means "as many as the data holds".
    pub rows: usize,
    /// When true a 1 bit means black; the default has 0 as black.
    pub black_is_1: bool,
    /// When true each row starts on a byte boundary.
    pub byte_align: bool,
}

impl Default for CcittParams {
    fn default() -> Self {
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            black_is_1: false,
            byte_align: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Bit reader
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    fn exhausted(&self) -> bool {
        self.bit >= self.total_bits()
    }

    fn read_bit(&mut self) -> Option<u8> {
        if self.exhausted() {
            return None;
        }
        let byte = self.data[self.bit >> 3];
        let value = (byte >> (7 - (self.bit & 7))) & 1;
        self.bit += 1;
        Some(value)
    }

    fn align_to_byte(&mut self) {
        self.bit = (self.bit + 7) & !7;
    }

    /// Consume an end-of-line code (eleven zeros and a one) if one is next.
    /// Some encoders pad with extra zeros ("fill") before it.
    fn skip_eol(&mut self) -> bool {
        let start = self.bit;
        let mut zeros = 0usize;
        loop {
            match self.read_bit() {
                Some(0) => {
                    zeros += 1;
                    if zeros > 64 {
                        self.bit = start;
                        return false;
                    }
                }
                Some(_) => {
                    if zeros >= 11 {
                        return true;
                    }
                    self.bit = start;
                    return false;
                }
                None => {
                    self.bit = start;
                    return false;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Modified Huffman run-length tables (ITU-T T.4)
// ---------------------------------------------------------------------------

/// (code bits, run length). Terminating codes are runs 0–63; makeup codes are
/// multiples of 64 and are always followed by a terminating code.
const WHITE_CODES: &[(&str, u16)] = &[
    ("00110101", 0), ("000111", 1), ("0111", 2), ("1000", 3),
    ("1011", 4), ("1100", 5), ("1110", 6), ("1111", 7),
    ("10011", 8), ("10100", 9), ("00111", 10), ("01000", 11),
    ("001000", 12), ("000011", 13), ("110100", 14), ("110101", 15),
    ("101010", 16), ("101011", 17), ("0100111", 18), ("0001100", 19),
    ("0001000", 20), ("0010111", 21), ("0000011", 22), ("0000100", 23),
    ("0101000", 24), ("0101011", 25), ("0010011", 26), ("0100100", 27),
    ("0011000", 28), ("00000010", 29), ("00000011", 30), ("00011010", 31),
    ("00011011", 32), ("00010010", 33), ("00010011", 34), ("00010100", 35),
    ("00010101", 36), ("00010110", 37), ("00010111", 38), ("00101000", 39),
    ("00101001", 40), ("00101010", 41), ("00101011", 42), ("00101100", 43),
    ("00101101", 44), ("00000100", 45), ("00000101", 46), ("00001010", 47),
    ("00001011", 48), ("01010010", 49), ("01010011", 50), ("01010100", 51),
    ("01010101", 52), ("00100100", 53), ("00100101", 54), ("01011000", 55),
    ("01011001", 56), ("01011010", 57), ("01011011", 58), ("01001010", 59),
    ("01001011", 60), ("00110010", 61), ("00110011", 62), ("00110100", 63),
    // Makeup codes.
    ("11011", 64), ("10010", 128), ("010111", 192), ("0110111", 256),
    ("00110110", 320), ("00110111", 384), ("01100100", 448), ("01100101", 512),
    ("01101000", 576), ("01100111", 640), ("011001100", 704), ("011001101", 768),
    ("011010010", 832), ("011010011", 896), ("011010100", 960), ("011010101", 1024),
    ("011010110", 1088), ("011010111", 1152), ("011011000", 1216), ("011011001", 1280),
    ("011011010", 1344), ("011011011", 1408), ("010011000", 1472), ("010011001", 1536),
    ("010011010", 1600), ("011000", 1664), ("010011011", 1728),
];

const BLACK_CODES: &[(&str, u16)] = &[
    ("0000110111", 0), ("010", 1), ("11", 2), ("10", 3),
    ("011", 4), ("0011", 5), ("0010", 6), ("00011", 7),
    ("000101", 8), ("000100", 9), ("0000100", 10), ("0000101", 11),
    ("0000111", 12), ("00000100", 13), ("00000111", 14), ("000011000", 15),
    ("0000010111", 16), ("0000011000", 17), ("0000001000", 18), ("00001100111", 19),
    ("00001101000", 20), ("00001101100", 21), ("00000110111", 22), ("00000101000", 23),
    ("00000010111", 24), ("00000011000", 25), ("000011001010", 26), ("000011001011", 27),
    ("000011001100", 28), ("000011001101", 29), ("000001101000", 30), ("000001101001", 31),
    ("000001101010", 32), ("000001101011", 33), ("000011010010", 34), ("000011010011", 35),
    ("000011010100", 36), ("000011010101", 37), ("000011010110", 38), ("000011010111", 39),
    ("000001101100", 40), ("000001101101", 41), ("000011011010", 42), ("000011011011", 43),
    ("000001010100", 44), ("000001010101", 45), ("000001010110", 46), ("000001010111", 47),
    ("000001100100", 48), ("000001100101", 49), ("000001010010", 50), ("000001010011", 51),
    ("000000100100", 52), ("000000110111", 53), ("000000111000", 54), ("000000100111", 55),
    ("000000101000", 56), ("000001011000", 57), ("000001011001", 58), ("000000101011", 59),
    ("000000101100", 60), ("000001011010", 61), ("000001100110", 62), ("000001100111", 63),
    // Makeup codes.
    ("0000001111", 64), ("000011001000", 128), ("000011001001", 192), ("000001011011", 256),
    ("000000110011", 320), ("000000110100", 384), ("000000110101", 448), ("0000001101100", 512),
    ("0000001101101", 576), ("0000001001010", 640), ("0000001001011", 704), ("0000001001100", 768),
    ("0000001001101", 832), ("0000001110010", 896), ("0000001110011", 960), ("0000001110100", 1024),
    ("0000001110101", 1088), ("0000001110110", 1152), ("0000001110111", 1216), ("0000001010010", 1280),
    ("0000001010011", 1344), ("0000001010100", 1408), ("0000001010101", 1472), ("0000001011010", 1536),
    ("0000001011011", 1600), ("0000001100100", 1664), ("0000001100101", 1728),
];

/// Makeup codes above 1728 are shared between both colours.
const EXTENDED_CODES: &[(&str, u16)] = &[
    ("00000001000", 1792), ("00000001100", 1856), ("00000001101", 1920),
    ("000000010010", 1984), ("000000010011", 2048), ("000000010100", 2112),
    ("000000010101", 2176), ("000000010110", 2240), ("000000010111", 2304),
    ("000000011100", 2368), ("000000011101", 2432), ("000000011110", 2496),
    ("000000011111", 2560),
];

/// Codes keyed by (bit length, value), which makes lookup a matter of reading
/// one more bit at a time until something matches — short codes are prefix-free
/// so the first match is the right one.
type CodeTable = HashMap<(u8, u32), u16>;

fn build(rows: &[&[(&str, u16)]]) -> CodeTable {
    let mut table = HashMap::new();
    for set in rows {
        for (bits, run) in set.iter() {
            let value = u32::from_str_radix(bits, 2).expect("code table is binary");
            table.insert((bits.len() as u8, value), *run);
        }
    }
    table
}

fn white_table() -> &'static CodeTable {
    static CELL: OnceLock<CodeTable> = OnceLock::new();
    CELL.get_or_init(|| build(&[WHITE_CODES, EXTENDED_CODES]))
}

fn black_table() -> &'static CodeTable {
    static CELL: OnceLock<CodeTable> = OnceLock::new();
    CELL.get_or_init(|| build(&[BLACK_CODES, EXTENDED_CODES]))
}

/// Longest code in either table.
const MAX_CODE_BITS: usize = 14;

fn read_code(reader: &mut BitReader, black: bool) -> Option<u16> {
    let table = if black { black_table() } else { white_table() };
    let mut value = 0u32;
    for length in 1..=MAX_CODE_BITS {
        value = (value << 1) | u32::from(reader.read_bit()?);
        if let Some(&run) = table.get(&(length as u8, value)) {
            return Some(run);
        }
    }
    None
}

/// A full run: any number of makeup codes followed by a terminating code.
fn read_run(reader: &mut BitReader, black: bool) -> Option<usize> {
    let mut total = 0usize;
    for _ in 0..64 {
        let run = read_code(reader, black)?;
        total += usize::from(run);
        if run < 64 {
            return Some(total);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Two-dimensional coding modes (T.4 §4.2.1.3 / T.6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Pass,
    Horizontal,
    /// a1 sits this far from b1, in the range -3..=3.
    Vertical(i64),
    /// End-of-line, end-of-facsimile-block, or an extension we do not read.
    EndOfData,
}

fn read_mode(reader: &mut BitReader) -> Option<Mode> {
    // V0 is a single 1 bit; everything else starts with at least one zero.
    if reader.read_bit()? == 1 {
        return Some(Mode::Vertical(0));
    }
    match reader.read_bit()? {
        1 => Some(if reader.read_bit()? == 1 {
            Mode::Vertical(1) // 011
        } else {
            Mode::Vertical(-1) // 010
        }),
        _ => match reader.read_bit()? {
            1 => Some(Mode::Horizontal), // 001
            _ => {
                if reader.read_bit()? == 1 {
                    return Some(Mode::Pass); // 0001
                }
                if reader.read_bit()? == 1 {
                    // 00001x
                    return Some(if reader.read_bit()? == 1 {
                        Mode::Vertical(2) // 000011
                    } else {
                        Mode::Vertical(-2) // 000010
                    });
                }
                if reader.read_bit()? == 1 {
                    // 000001x
                    return Some(if reader.read_bit()? == 1 {
                        Mode::Vertical(3) // 0000011
                    } else {
                        Mode::Vertical(-3) // 0000010
                    });
                }
                // Six or more leading zeros: an extension or EOL. Either way
                // this row is over.
                Some(Mode::EndOfData)
            }
        },
    }
}

/// `b1` and `b2` for the current position: the next changing element on the
/// reference line with colour opposite to the current colour, and the one
/// after it.
fn find_b(reference: &[usize], a0: i64, black: bool, columns: usize) -> (usize, usize) {
    // Reference transitions alternate starting with white→black, so an
    // even index is a change *to* black and an odd index a change to white.
    let want_even = !black;
    let mut i = 0;
    while i < reference.len() {
        if reference[i] as i64 > a0 && ((i % 2 == 0) == want_even) {
            break;
        }
        i += 1;
    }
    (
        reference.get(i).copied().unwrap_or(columns),
        reference.get(i + 1).copied().unwrap_or(columns),
    )
}

/// Decode one two-dimensionally coded row into changing-element positions.
fn decode_2d_row(
    reader: &mut BitReader,
    reference: &[usize],
    columns: usize,
    out: &mut Vec<usize>,
) -> bool {
    out.clear();
    let mut a0: i64 = -1;
    let mut black = false;

    while (a0 as i64) < columns as i64 {
        let (b1, b2) = find_b(reference, a0, black, columns);
        let Some(mode) = read_mode(reader) else {
            return false;
        };
        match mode {
            Mode::Pass => {
                a0 = b2 as i64;
            }
            Mode::Horizontal => {
                let (Some(first), Some(second)) =
                    (read_run(reader, black), read_run(reader, !black))
                else {
                    return false;
                };
                let start = a0.max(0) as usize;
                let a1 = start.saturating_add(first).min(columns);
                let a2 = a1.saturating_add(second).min(columns);
                out.push(a1);
                out.push(a2);
                a0 = a2 as i64;
                if a1 == a2 && a2 == columns {
                    break;
                }
            }
            Mode::Vertical(delta) => {
                let a1 = (b1 as i64 + delta).clamp(0, columns as i64) as usize;
                out.push(a1);
                a0 = a1 as i64;
                black = !black;
            }
            Mode::EndOfData => return false,
        }
        if out.len() > columns + 2 {
            return false; // runaway row
        }
    }
    true
}

/// Decode one one-dimensionally coded (Modified Huffman) row.
fn decode_1d_row(reader: &mut BitReader, columns: usize, out: &mut Vec<usize>) -> bool {
    out.clear();
    let mut pos = 0usize;
    let mut black = false;
    while pos < columns {
        let Some(run) = read_run(reader, black) else {
            return false;
        };
        pos = pos.saturating_add(run).min(columns);
        out.push(pos);
        black = !black;
        if out.len() > columns + 2 {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Decode a CCITT stream into packed 1-bit-per-pixel rows, each row padded to
/// a whole number of bytes — the layout the raw image path already expects for
/// `/BitsPerComponent 1`.
///
/// Returns `None` only when nothing at all could be read.
pub fn decode(data: &[u8], params: &CcittParams) -> Option<Vec<u8>> {
    let columns = params.columns.clamp(1, 1 << 16);
    let row_bytes = columns.div_ceil(8);
    // A page taller than this is damage, not a document.
    let max_rows = if params.rows > 0 {
        params.rows
    } else {
        20_000.min(data.len() * 8)
    };

    let mut reader = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(row_bytes * max_rows.min(4096));

    let mut reference: Vec<usize> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut rows_done = 0usize;

    // Bit values written for each colour. With the default /BlackIs1 false, a
    // zero bit is black, which is also how DeviceGray reads 1-bit samples.
    let (black_bit, white_bit) = if params.black_is_1 { (1u8, 0u8) } else { (0u8, 1u8) };

    while rows_done < max_rows {
        if params.byte_align && params.k >= 0 {
            reader.align_to_byte();
        }
        // Group 3 rows may be preceded by an EOL; Group 4 normally has none,
        // but an EOFB pair ends the image.
        let had_eol = reader.skip_eol();
        if reader.exhausted() {
            break;
        }

        let two_dimensional = if params.k < 0 {
            true
        } else if params.k == 0 {
            false
        } else if had_eol {
            // Mixed mode: one flag bit after each EOL says how the row is coded.
            match reader.read_bit() {
                Some(bit) => bit == 0,
                None => break,
            }
        } else {
            false
        };

        if params.byte_align && params.k < 0 {
            // T.6 with /EncodedByteAlign aligns before each row's data.
            reader.align_to_byte();
        }

        let ok = if two_dimensional {
            decode_2d_row(&mut reader, &reference, columns, &mut current)
        } else {
            decode_1d_row(&mut reader, columns, &mut current)
        };
        if !ok {
            break;
        }

        // Paint the changing elements into packed bits.
        let base = out.len();
        out.resize(base + row_bytes, 0);
        let mut pos = 0usize;
        let mut black = false;
        let paint = |from: usize, to: usize, black: bool, out: &mut Vec<u8>| {
            let bit = if black { black_bit } else { white_bit };
            if bit == 0 {
                return; // buffer starts zeroed
            }
            for x in from..to {
                out[base + (x >> 3)] |= 0x80 >> (x & 7);
            }
        };
        for &change in current.iter() {
            let change = change.min(columns);
            paint(pos, change, black, &mut out);
            pos = change;
            black = !black;
        }
        paint(pos, columns, black, &mut out);

        std::mem::swap(&mut reference, &mut current);
        rows_done += 1;
    }

    if rows_done == 0 {
        return None;
    }

    // A truncated stream leaves the rest of the image white rather than short.
    if params.rows > 0 && rows_done < params.rows {
        let missing = (params.rows - rows_done) * row_bytes;
        let fill = if params.black_is_1 { 0x00 } else { 0xFF };
        out.extend(std::iter::repeat(fill).take(missing));
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Group 4 encoder, enough to round-trip the decoder against
    /// known images rather than against hand-copied bit strings.
    struct Encoder {
        bits: Vec<u8>,
    }

    impl Encoder {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }
        fn push(&mut self, code: &str) {
            for ch in code.chars() {
                self.bits.push(if ch == '1' { 1 } else { 0 });
            }
        }
        fn run_code(&mut self, mut run: usize, black: bool) {
            let set = if black { BLACK_CODES } else { WHITE_CODES };
            // Emit makeup codes (largest first) then the terminating code.
            while run >= 64 {
                let makeup = (run / 64) * 64;
                let makeup = makeup.min(1728);
                let code = set
                    .iter()
                    .find(|(_, r)| *r as usize == makeup)
                    .expect("makeup code exists");
                self.push(code.0);
                run -= makeup;
            }
            let code = set
                .iter()
                .find(|(_, r)| *r as usize == run && r < &64)
                .expect("terminating code exists");
            self.push(code.0);
        }
        fn finish(self) -> Vec<u8> {
            let mut bits = self.bits;
            while bits.len() % 8 != 0 {
                bits.push(0);
            }
            bits.chunks(8)
                .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b))
                .collect()
        }
    }

    /// Encode rows (true = black) as Group 4, using horizontal mode
    /// throughout, which every conforming decoder must handle.
    fn encode_g4_horizontal(rows: &[Vec<bool>], columns: usize) -> Vec<u8> {
        let mut enc = Encoder::new();
        for row in rows {
            let mut x = 0usize;
            let mut black = false;
            while x < columns {
                let mut first = 0usize;
                while x + first < columns && row[x + first] == black {
                    first += 1;
                }
                let mut second = 0usize;
                while x + first + second < columns && row[x + first + second] == !black {
                    second += 1;
                }
                enc.push("001"); // horizontal mode
                enc.run_code(first, black);
                enc.run_code(second, !black);
                x += first + second;
            }
        }
        enc.finish()
    }

    fn unpack(data: &[u8], columns: usize, rows: usize, black_is_1: bool) -> Vec<Vec<bool>> {
        let row_bytes = columns.div_ceil(8);
        (0..rows)
            .map(|y| {
                (0..columns)
                    .map(|x| {
                        let byte = data[y * row_bytes + (x >> 3)];
                        let bit = (byte >> (7 - (x & 7))) & 1;
                        if black_is_1 { bit == 1 } else { bit == 0 }
                    })
                    .collect()
            })
            .collect()
    }

    fn sample_rows(columns: usize) -> Vec<Vec<bool>> {
        vec![
            vec![false; columns],
            (0..columns).map(|x| x >= 10 && x < 40).collect(),
            (0..columns).map(|x| x % 2 == 0).collect(),
            vec![true; columns],
            (0..columns).map(|x| x >= columns - 5).collect(),
        ]
    }

    #[test]
    fn group4_horizontal_round_trip() {
        let columns = 64;
        let rows = sample_rows(columns);
        let encoded = encode_g4_horizontal(&rows, columns);
        let params = CcittParams {
            k: -1,
            columns,
            rows: rows.len(),
            ..CcittParams::default()
        };
        let decoded = decode(&encoded, &params).expect("decodes");
        assert_eq!(unpack(&decoded, columns, rows.len(), false), rows);
    }

    #[test]
    fn group3_1d_round_trip() {
        let columns = 64;
        let rows = sample_rows(columns);
        // G3 1-D is just the run codes, no mode prefix.
        let mut enc = Encoder::new();
        for row in &rows {
            let mut x = 0usize;
            let mut black = false;
            while x < columns {
                let mut run = 0usize;
                while x + run < columns && row[x + run] == black {
                    run += 1;
                }
                enc.run_code(run, black);
                x += run;
                black = !black;
            }
        }
        let encoded = enc.finish();
        let params = CcittParams {
            k: 0,
            columns,
            rows: rows.len(),
            ..CcittParams::default()
        };
        let decoded = decode(&encoded, &params).expect("decodes");
        assert_eq!(unpack(&decoded, columns, rows.len(), false), rows);
    }

    #[test]
    fn black_is_1_inverts_the_output_bits() {
        let columns = 32;
        let rows = vec![(0..columns).map(|x| x < 16).collect::<Vec<bool>>()];
        let encoded = encode_g4_horizontal(&rows, columns);
        for black_is_1 in [false, true] {
            let params = CcittParams {
                k: -1,
                columns,
                rows: 1,
                black_is_1,
                ..CcittParams::default()
            };
            let decoded = decode(&encoded, &params).expect("decodes");
            assert_eq!(
                unpack(&decoded, columns, 1, black_is_1),
                rows,
                "black_is_1={black_is_1}"
            );
        }
    }

    #[test]
    fn wide_runs_use_makeup_codes() {
        let columns = 1728;
        let rows = vec![(0..columns).map(|x| x >= 800).collect::<Vec<bool>>()];
        let encoded = encode_g4_horizontal(&rows, columns);
        let params = CcittParams {
            k: -1,
            columns,
            rows: 1,
            ..CcittParams::default()
        };
        let decoded = decode(&encoded, &params).expect("decodes");
        assert_eq!(unpack(&decoded, columns, 1, false), rows);
    }

    #[test]
    fn truncated_data_keeps_the_rows_it_managed() {
        let columns = 64;
        let rows = sample_rows(columns);
        let encoded = encode_g4_horizontal(&rows, columns);
        let params = CcittParams {
            k: -1,
            columns,
            rows: rows.len(),
            ..CcittParams::default()
        };
        let decoded = decode(&encoded[..encoded.len() / 2], &params).expect("partial decode");
        // Still a full-size buffer, so the image geometry stays correct.
        assert_eq!(decoded.len(), columns.div_ceil(8) * rows.len());
        let unpacked = unpack(&decoded, columns, rows.len(), false);
        assert_eq!(unpacked[0], rows[0], "early rows survive truncation");
    }

    #[test]
    fn undecodable_data_returns_none() {
        let params = CcittParams {
            k: -1,
            columns: 64,
            rows: 4,
            ..CcittParams::default()
        };
        // No data, and data that is nothing but leading zeros, both fail to
        // produce even one row.
        assert!(decode(&[], &params).is_none());
        assert!(decode(&[0x00; 8], &params).is_none());
    }

    #[test]
    fn all_ones_is_a_valid_run_of_blank_rows() {
        // Every bit set reads as vertical-zero mode against an empty
        // reference line, which is a legitimate way to say "this row is
        // entirely white" — not something to reject.
        let params = CcittParams {
            k: -1,
            columns: 64,
            rows: 4,
            ..CcittParams::default()
        };
        let decoded = decode(&[0xFF; 8], &params).expect("blank rows decode");
        assert_eq!(decoded.len(), 8 * 4);
        assert!(
            unpack(&decoded, 64, 4, false).iter().all(|r| r.iter().all(|&b| !b)),
            "every pixel should be white"
        );
    }

    #[test]
    fn code_tables_are_prefix_free() {
        for table in [white_table(), black_table()] {
            let codes: Vec<(u8, u32)> = table.keys().copied().collect();
            for &(len_a, val_a) in &codes {
                for &(len_b, val_b) in &codes {
                    if (len_a, val_a) == (len_b, val_b) || len_a >= len_b {
                        continue;
                    }
                    let shifted = val_b >> (len_b - len_a);
                    assert_ne!(
                        shifted, val_a,
                        "code {val_a:b} ({len_a} bits) prefixes {val_b:b} ({len_b} bits)"
                    );
                }
            }
        }
    }
}
