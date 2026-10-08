//! A TrueType font with no ink: two empty glyphs, each half an em wide.
//!
//! The invisible text layer embeds it so that every viewer, including this
//! library's own renderer and text-geometry code, measures the layer's
//! glyphs from the PDF's widths alone (no substitute outlines widening the
//! selection boxes), and so that the font is embedded, as archival profiles
//! expect. It is generated rather than shipped as a file: all ten tables,
//! with correct checksums, in about 470 bytes.

fn be16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

pub fn font_program() -> Vec<u8> {
    let mut head = Vec::new();
    be32(&mut head, 0x0001_0000); // version
    be32(&mut head, 0x0001_0000); // font revision
    be32(&mut head, 0); // checksum adjustment, patched below
    be32(&mut head, 0x5F0F_3CF5); // magic
    be16(&mut head, 0x000B); // flags: baseline and left sidebearing at 0, integer ppem
    be16(&mut head, 1000); // units per em
    head.extend_from_slice(&[0; 16]); // created, modified
    for v in [0i16, -200, 500, 800] {
        be16(&mut head, v as u16); // bounding box
    }
    for v in [0u16, 3, 2, 0, 0] {
        be16(&mut head, v); // style, smallest ppem, direction hint, short loca, glyph format
    }

    let mut hhea = Vec::new();
    be32(&mut hhea, 0x0001_0000);
    for v in [800i16, -200, 0] {
        be16(&mut hhea, v as u16); // ascender, descender, line gap
    }
    for v in [500u16, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2] {
        be16(&mut hhea, v); // max advance .. metric format, then two h-metrics
    }

    let mut hmtx = Vec::new();
    for _ in 0..2 {
        be16(&mut hmtx, 500);
        be16(&mut hmtx, 0);
    }

    let mut maxp = Vec::new();
    be32(&mut maxp, 0x0001_0000);
    for v in [2u16, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0] {
        be16(&mut maxp, v); // two glyphs, two zones, nothing else
    }

    let loca = vec![0u8; 6]; // three short offsets: both glyphs are empty
    let glyf = Vec::new();

    let mut cmap = Vec::new();
    for v in [0u16, 1, 3, 1] {
        be16(&mut cmap, v); // version, one table: Windows Unicode BMP
    }
    be32(&mut cmap, 12);
    for v in [4u16, 24, 0, 2, 2, 0, 0, 0xFFFF, 0, 0xFFFF, 1, 0] {
        be16(&mut cmap, v); // format 4 holding only the closing segment
    }

    let names = [
        (1u16, "GlyphLessFont"),
        (2, "Regular"),
        (3, "GlyphLessFont"),
        (4, "GlyphLessFont"),
        (5, "Version 1.0"),
        (6, "GlyphLessFont"),
    ];
    let mut name = Vec::new();
    let mut strings = Vec::new();
    be16(&mut name, 0);
    be16(&mut name, names.len() as u16);
    be16(&mut name, 6 + 12 * names.len() as u16);
    for (id, text) in names {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        for v in [
            3u16,
            1,
            0x0409,
            id,
            utf16.len() as u16,
            strings.len() as u16,
        ] {
            be16(&mut name, v);
        }
        strings.extend(utf16);
    }
    name.extend(strings);

    let mut post = Vec::new();
    be32(&mut post, 0x0003_0000); // no glyph names
    be32(&mut post, 0);
    be16(&mut post, (-100i16) as u16);
    be16(&mut post, 50);
    for v in [1u32, 0, 0, 0, 0] {
        be32(&mut post, v);
    }

    let mut os2 = Vec::new();
    for v in [4u16, 500, 400, 5, 0] {
        be16(&mut os2, v); // version 4, average width, regular, medium, installable
    }
    os2.extend_from_slice(&[0; 20 + 2 + 10 + 16]); // sub/superscripts, class, panose, ranges
    os2.extend_from_slice(b"NONE");
    for v in [0x0040u16, 0x20, 0x20, 800, (-200i16) as u16, 0, 800, 200] {
        be16(&mut os2, v);
    }
    be32(&mut os2, 1);
    be32(&mut os2, 0);
    for v in [500u16, 800, 0, 32, 0] {
        be16(&mut os2, v);
    }

    let tables: [(&[u8; 4], Vec<u8>); 10] = [
        (b"OS/2", os2),
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
        (b"name", name),
        (b"post", post),
    ];
    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000);
    for v in [10u16, 128, 3, 32] {
        be16(&mut out, v); // table count, search range, entry selector, range shift
    }
    let directory_end = 12 + 16 * tables.len();
    let mut body = Vec::new();
    let mut head_at = 0;
    for (tag, data) in &tables {
        let offset = directory_end + body.len();
        if *tag == b"head" {
            head_at = offset;
        }
        out.extend_from_slice(*tag);
        be32(&mut out, checksum(data));
        be32(&mut out, offset as u32);
        be32(&mut out, data.len() as u32);
        body.extend_from_slice(data);
        body.resize(body.len().div_ceil(4) * 4, 0);
    }
    out.extend(body);
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
    out[head_at + 8..head_at + 12].copy_from_slice(&adjustment.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_parses_and_has_two_empty_glyphs() {
        let program = font_program();
        assert_eq!(checksum(&program), 0xB1B0_AFBA, "whole-font checksum");
        let font = pdf_render::font::truetype::TrueTypeFont::parse(program).expect("parses");
        assert_eq!(font.num_glyphs(), 2);
        assert!(font.has_outlines());
        assert!(font.glyph_outline(1).is_none() && font.glyph_outline(0).is_none());
        assert_eq!(font.advance(1), Some(500.0));
    }
}
