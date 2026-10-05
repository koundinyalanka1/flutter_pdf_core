//! Picture-codec adapters. Validate metadata before entering allocation-heavy
//! decoding; these bounds are not a general-purpose process memory sandbox.

use super::*;

pub(super) const MAX_CODEC_BYTES: usize = 32 * 1024 * 1024;
const MAX_JPX_SAMPLES: usize = 32_000_000;
const MAX_SEGMENTS: usize = 4096;

fn be32(data: &[u8], offset: usize) -> Option<usize> {
    usize::try_from(u32::from_be_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
    .ok()
}

/// Find the codestream without allocating according to untrusted JP2 boxes.
fn jpx_codestream(data: &[u8]) -> Option<&[u8]> {
    if data.starts_with(b"\xff\x4f\xff\x51") {
        return Some(data);
    }
    let mut cursor = 0;
    while cursor < data.len() {
        let start = cursor;
        let mut len = be32(data, start)?;
        let kind = data.get(start + 4..start + 8)?;
        let header = if len == 1 {
            len = usize::try_from(u64::from_be_bytes(
                data.get(start + 8..start + 16)?.try_into().ok()?,
            ))
            .ok()?;
            16
        } else {
            8
        };
        if len == 0 {
            len = data.len() - start;
        }
        if len < header {
            return None;
        }
        cursor = start.checked_add(len)?;
        let payload = data.get(start + header..cursor)?;
        if kind == b"jp2c" {
            return Some(payload);
        }
    }
    None
}

fn check_jpx(data: &[u8]) -> Option<()> {
    if data.len() > MAX_CODEC_BYTES {
        return None;
    }
    let data = jpx_codestream(data)?;
    if !data.starts_with(b"\xff\x4f\xff\x51") {
        return None;
    }
    let xs = be32(data, 8)?;
    let ys = be32(data, 12)?;
    let width = xs.checked_sub(be32(data, 16)?)?;
    let height = ys.checked_sub(be32(data, 20)?)?;
    let pixels = image_pixel_count(width, height)?;
    let components = usize::from(u16::from_be_bytes(data.get(40..42)?.try_into().ok()?));
    if components == 0 || components > 5 || pixels.checked_mul(components)? > MAX_JPX_SAMPLES {
        return None;
    }
    let tw = be32(data, 24)?;
    let th = be32(data, 28)?;
    if tw == 0 || th == 0 {
        return None;
    }
    let columns = xs.checked_sub(be32(data, 32)?)?.div_ceil(tw);
    let rows = ys.checked_sub(be32(data, 36)?)?.div_ceil(th);
    if columns.checked_mul(rows)? > MAX_SEGMENTS {
        return None;
    }
    Some(())
}

pub(super) fn decode_jpx(
    doc: &PdfDocument,
    dict: &Dictionary,
    data: &[u8],
    is_stencil: bool,
    decode_array: Option<&[f64]>,
) -> Option<DecodedImage> {
    // Keep an unexpected codec assertion local to this image: the caller can
    // report a render warning and continue painting the rest of the page.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_jpx_inner(doc, dict, data, is_stencil, decode_array)
    }))
    .ok()
    .flatten()
}

fn decode_jpx_inner(
    doc: &PdfDocument,
    dict: &Dictionary,
    data: &[u8],
    is_stencil: bool,
    decode_array: Option<&[f64]>,
) -> Option<DecodedImage> {
    check_jpx(data)?;
    let pdf_space = color_space(doc, dict);
    let indexed = matches!(pdf_space, Some(ColorSpace::Indexed(..)));
    let settings = hayro_jpeg2000::DecodeSettings {
        resolve_palette_indices: !indexed,
        ..Default::default()
    };
    let image = hayro_jpeg2000::Image::new(data, &settings).ok()?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    let pixels = image_pixel_count(width, height)?;
    let codec_channels = image.color_space().num_channels() as usize;
    let count = codec_channels.checked_add(usize::from(image.has_alpha()))?;
    if pixels.checked_mul(count)? > MAX_JPX_SAMPLES {
        return None;
    }
    let space = pdf_space.unwrap_or(match codec_channels {
        1 => ColorSpace::Gray,
        3 => ColorSpace::Rgb,
        4 => ColorSpace::Cmyk,
        _ => return None,
    });
    let channels = space.components();
    // Raw four-component codestreams have no color metadata. The PDF's CMYK
    // declaration takes precedence over the decoder's RGB+alpha guess.
    if count < channels || count > channels + 1 {
        return None;
    }
    let has_alpha = count == channels + 1;
    let mut context = hayro_jpeg2000::DecoderContext::default();
    let decoded = image.decode(&mut context).ok()?;
    let components = decoded.components();
    if components.len() != count || components.iter().any(|c| c.samples().len() != pixels) {
        return None;
    }
    let alpha_mode = dict_int(doc, dict, "SMaskInData", "SMaskInData").unwrap_or(0);
    let use_alpha = has_alpha && matches!(alpha_mode, 1 | 2);
    let mut rgb = if is_stencil {
        Vec::new()
    } else {
        Vec::with_capacity(pixels * 3)
    };
    let mut alpha = if use_alpha || is_stencil {
        Vec::with_capacity(pixels)
    } else {
        Vec::new()
    };
    let normalized = |component: usize, pixel: usize| -> f64 {
        let component = &components[component];
        let maximum = 2.0_f64.powi(i32::from(component.bit_depth())) - 1.0;
        (f64::from(component.samples()[pixel]) / maximum).clamp(0.0, 1.0)
    };
    for pixel in 0..pixels {
        if is_stencil {
            let mut value = normalized(0, pixel);
            if let Some([a, b, ..]) = decode_array {
                value = a + value * (b - a);
            }
            alpha.push(if value < 0.5 { 255 } else { 0 });
            continue;
        }
        let opacity = if use_alpha {
            normalized(channels, pixel)
        } else {
            1.0
        };
        let mut values = [0.0; 4];
        for c in 0..channels {
            values[c] = normalized(c, pixel);
        }
        // PDF Decode arrays are ignored for JPX; Indexed retains the original
        // component value instead of the codec's 8-bit display conversion.
        let raw_index = components[0].samples()[pixel].max(0.0).round() as u32;
        let mut color = space.to_rgb(&values[..channels], raw_index);
        if use_alpha && alpha_mode == 2 && opacity > 0.0 {
            color.r /= opacity as f32;
            color.g /= opacity as f32;
            color.b /= opacity as f32;
        }
        rgb.extend([color.r, color.g, color.b].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8));
        if use_alpha {
            alpha.push((opacity * 255.0).round() as u8);
        }
    }
    Some(DecodedImage {
        width,
        height,
        rgb,
        alpha,
        is_stencil,
    })
}

/// Validate embedded segment framing and explicit region allocations before
/// the codec allocates reference arrays/bitmaps. Symbol dimensions themselves
/// are entropy-coded and additionally checked by the decoder.
fn check_jbig2(
    data: &[u8],
    segments: &mut usize,
    region_pixels: &mut usize,
    symbol_count: &mut usize,
) -> Option<()> {
    if data.len() > MAX_CODEC_BYTES {
        return None;
    }
    let mut cursor: usize = 0;
    while cursor < data.len() {
        *segments = segments.checked_add(1)?;
        if *segments > MAX_SEGMENTS {
            return None;
        }
        let number = be32(data, cursor)?;
        let flags = *data.get(cursor + 4)?;
        let kind = flags & 63;
        let short = *data.get(cursor + 5)? >> 5;
        let refs;
        if short == 7 {
            refs = be32(data, cursor + 5)? & 0x1fff_ffff;
            if refs > MAX_SEGMENTS {
                return None;
            }
            cursor = cursor.checked_add(9 + (refs + 1).div_ceil(8))?;
        } else {
            if short > 4 {
                return None;
            }
            refs = short as usize;
            cursor += 6;
        }
        let ref_size = if number <= 256 {
            1
        } else if number <= 65536 {
            2
        } else {
            4
        };
        cursor = cursor.checked_add(refs.checked_mul(ref_size)?)?;
        cursor = cursor.checked_add(if flags & 64 == 0 { 1 } else { 4 })?;
        let mut len = be32(data, cursor)?;
        cursor += 4;
        if len == u32::MAX as usize {
            if kind != 38 {
                return None;
            }
            let tail = data.get(cursor..)?;
            let marker: &[u8] = if tail.get(17)? & 1 == 1 {
                &[0, 0]
            } else {
                &[255, 172]
            };
            len = 18 + tail.get(18..)?.windows(6).position(|w| &w[..2] == marker)? + 6;
        }
        let payload = data.get(cursor..cursor.checked_add(len)?)?;
        cursor += len;
        if matches!(
            kind,
            4 | 6 | 7 | 20 | 22 | 23 | 36 | 38 | 39 | 40 | 42 | 43 | 48
        ) {
            let width = be32(payload, 0)?;
            let height = be32(payload, 4)?;
            if !(kind == 48 && height == u32::MAX as usize) {
                let count = image_pixel_count(width, height)?;
                *region_pixels = region_pixels.checked_add(count)?;
                if *region_pixels > MAX_IMAGE_PIXELS * 4 {
                    return None;
                }
            } else if width == 0 || width > MAX_IMAGE_PIXELS {
                return None;
            }
        }
        if matches!(kind, 20 | 22 | 23) {
            let count = image_pixel_count(be32(payload, 18)?, be32(payload, 22)?)?;
            *region_pixels = region_pixels.checked_add(count)?;
            if *region_pixels > MAX_IMAGE_PIXELS * 4 {
                return None;
            }
        }
        if kind == 0 {
            let flags = u16::from_be_bytes(payload.get(..2)?.try_into().ok()?);
            let huffman = flags & 1 != 0;
            let refinement = flags & 2 != 0;
            let template = (flags >> 10) & 3;
            let mut offset = 2;
            if !huffman {
                offset += if template == 0 { 8 } else { 2 };
            }
            if refinement && flags & 0x1000 == 0 {
                offset += 4;
            }
            *symbol_count = symbol_count
                .checked_add(be32(payload, offset)?)?
                .checked_add(be32(payload, offset + 4)?)?;
            if *symbol_count > 65_535 {
                return None;
            }
        }
        if kind == 16 {
            let count = be32(payload, 3)?.checked_add(1)?;
            let pixels = count
                .checked_mul(*payload.get(1)? as usize)?
                .checked_mul(*payload.get(2)? as usize)?;
            if count > 65_536 || pixels > MAX_IMAGE_PIXELS {
                return None;
            }
        }
    }
    Some(())
}

pub(super) fn decode_jbig2(data: &[u8], globals: Option<&[u8]>) -> Option<(Vec<u8>, usize, usize)> {
    std::panic::catch_unwind(|| decode_jbig2_inner(data, globals))
        .ok()
        .flatten()
}

fn decode_jbig2_inner(data: &[u8], globals: Option<&[u8]>) -> Option<(Vec<u8>, usize, usize)> {
    let mut segments = 0;
    let mut region_pixels = 0;
    let mut symbol_count = 0;
    if let Some(globals) = globals {
        check_jbig2(
            globals,
            &mut segments,
            &mut region_pixels,
            &mut symbol_count,
        )?;
    }
    check_jbig2(data, &mut segments, &mut region_pixels, &mut symbol_count)?;
    let image = hayro_jbig2::Image::new_embedded(data, globals).ok()?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    image_pixel_count(width, height)?;
    let mut output = BilevelOutput {
        bytes: vec![0; width.div_ceil(8).checked_mul(height)?],
        width,
        height,
        x: 0,
        y: 0,
        invalid: false,
    };
    image.decode(&mut output).ok()?;
    if output.invalid || output.y != height || output.x != 0 {
        return None;
    }
    Some((output.bytes, width, height))
}

struct BilevelOutput {
    bytes: Vec<u8>,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    invalid: bool,
}

impl hayro_jbig2::Decoder for BilevelOutput {
    fn push_pixel(&mut self, black: bool) {
        if self.x >= self.width || self.y >= self.height {
            self.invalid = true;
            return;
        }
        // JBIG2's black=true becomes PDF DeviceGray sample zero.
        if !black {
            self.bytes[self.y * self.width.div_ceil(8) + self.x / 8] |= 128 >> (self.x % 8);
        }
        self.x += 1;
    }
    fn push_pixel_chunk(&mut self, black: bool, count: u32) {
        let Some(count) = (count as usize).checked_mul(8) else {
            self.invalid = true;
            return;
        };
        if count > self.width.saturating_sub(self.x) || self.y >= self.height || self.x % 8 != 0 {
            self.invalid = true;
            return;
        }
        let start = self.y * self.width.div_ceil(8) + self.x / 8;
        self.bytes[start..start + count / 8].fill(if black { 0 } else { 255 });
        self.x += count;
    }
    fn next_line(&mut self) {
        if self.x != self.width || self.y >= self.height {
            self.invalid = true;
        }
        self.x = 0;
        self.y = self.y.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: &[u8] = include_bytes!("../tests/fixtures/codecs/indexed-small.jp2");
    const RGBA: &[u8] = include_bytes!("../tests/fixtures/codecs/rgba-u4.jp2");
    const SYMBOLS: &[u8] = include_bytes!("../tests/fixtures/codecs/bitmap-symbol-global.jbig2");

    fn stream(data: &[u8], filter: &str) -> PdfStream {
        let mut dict = Dictionary::new();
        // Deliberately wrong: allocation and output use the codec header.
        dict.insert("Width".into(), PdfObject::Integer(1));
        dict.insert("Height".into(), PdfObject::Integer(1));
        dict.insert("Filter".into(), PdfObject::Name(filter.into()));
        PdfStream::new(dict, data.to_vec())
    }

    #[test]
    fn jpx_palette_without_pdf_colorspace_uses_container_colors() {
        let doc = PdfDocument::new_empty("1.7");
        let image = decode_image(&doc, &stream(PALETTE, "JPXDecode")).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        // This fixture's six palette entries are red, green, blue, cyan,
        // magenta and yellow, arranged in palette order.
        assert_eq!(
            image.rgb,
            [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0]
        );
    }

    #[test]
    fn jpx_pdf_palette_keeps_indices_instead_of_resolving_embedded_palette() {
        let doc = PdfDocument::new_empty("1.7");
        let mut stream = stream(PALETTE, "JPXDecode");
        stream.dictionary.insert(
            "ColorSpace".into(),
            PdfObject::Array(vec![
                PdfObject::Name("Indexed".into()),
                PdfObject::Name("DeviceGray".into()),
                PdfObject::Integer(5),
                PdfObject::HexString(vec![10, 20, 30, 40, 50, 60]),
            ]),
        );
        let image = decode_image(&doc, &stream).unwrap();
        assert_eq!(
            image.rgb,
            [10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40, 50, 50, 50, 60, 60, 60]
        );
    }

    #[test]
    fn jpx_embedded_four_bit_alpha_obeys_smaskindata() {
        let doc = PdfDocument::new_empty("1.7");
        let mut stream = stream(RGBA, "JPXDecode");
        let opaque = decode_image(&doc, &stream).unwrap();
        assert!(opaque.alpha.is_empty());
        stream
            .dictionary
            .insert("SMaskInData".into(), PdfObject::Integer(1));
        let image = decode_image(&doc, &stream).unwrap();
        assert_eq!((image.width, image.height), (119, 101));
        assert_eq!(image.alpha.len(), 119 * 101);
        assert_eq!(&image.alpha[..6], &[17; 6]);
        assert_eq!(&image.alpha[6..14], &[34; 8]);
        assert!(image.alpha.contains(&255));
        assert_eq!(opaque.rgb, image.rgb);
    }

    #[test]
    fn jpx_raw_codestream_and_wrapped_byte_filters_decode() {
        let doc = PdfDocument::new_empty("1.7");
        let code = jpx_codestream(PALETTE).unwrap();
        let image = decode_image(&doc, &stream(code, "JPXDecode")).unwrap();
        assert_eq!(
            image.rgb,
            [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 5]
        );
        let hex: String = PALETTE.iter().map(|b| format!("{b:02x}")).collect();
        let mut stream = stream(format!("{hex}>").as_bytes(), "JPXDecode");
        stream.dictionary.insert(
            "Filter".into(),
            PdfObject::Array(vec![
                PdfObject::Name("ASCIIHexDecode".into()),
                PdfObject::Name("JPXDecode".into()),
            ]),
        );
        assert_eq!(decode_image(&doc, &stream).unwrap().rgb.len(), 18);
    }

    #[test]
    fn jpx_raw_four_component_pdf_cmyk_overrides_codec_alpha_guess() {
        let doc = PdfDocument::new_empty("1.7");
        let mut stream = stream(jpx_codestream(RGBA).unwrap(), "JPXDecode");
        stream
            .dictionary
            .insert("ColorSpace".into(), PdfObject::Name("DeviceCMYK".into()));
        let image = decode_image(&doc, &stream).unwrap();
        assert!(image.alpha.is_empty());
        // Four-bit samples at the first pixel are C=M=Y=0, K=1.
        assert_eq!(&image.rgb[..3], &[238, 238, 238]);
    }

    #[test]
    fn jbig2_globals_match_every_pixel_of_independent_reference_bitmap() {
        // Strip the standalone file header and separate the global symbol
        // dictionary (page association 0) from the embedded page segments.
        let mut doc = PdfDocument::new_empty("1.7");
        let global_id = doc.add_object(PdfObject::Stream(PdfStream::new(
            Dictionary::new(),
            SYMBOLS[13..300].to_vec(),
        )));
        let mut stream = stream(&SYMBOLS[300..], "JBIG2Decode");
        let mut parms = Dictionary::new();
        parms.insert("JBIG2Globals".into(), PdfObject::Reference(global_id));
        stream
            .dictionary
            .insert("DecodeParms".into(), PdfObject::Dictionary(parms));
        let image = decode_image(&doc, &stream).unwrap();
        assert_eq!((image.width, image.height), (399, 400));
        let reference = include_bytes!("../tests/fixtures/codecs/bitmap.bmp");
        // Original BMP is bottom-up, 1 bpp, palette 0=white, 1=black,
        // with each row padded to a multiple of four bytes.
        for y in 0..400 {
            for x in 0..399 {
                let black = reference[62 + (399 - y) * 52 + x / 8] & (128 >> (x % 8)) != 0;
                assert_eq!(
                    image.rgb[(y * 399 + x) * 3],
                    if black { 0 } else { 255 },
                    "at {x},{y}"
                );
            }
        }
        // DecodeParms must be aligned with its filter, not taken from the
        // first entry in a chain that starts with an ordinary byte filter.
        let hex: String = stream.data.iter().map(|b| format!("{b:02x}")).collect();
        stream.data = format!("{hex}>").into_bytes();
        stream.dictionary.insert(
            "Filter".into(),
            PdfObject::Array(vec![
                PdfObject::Name("ASCIIHexDecode".into()),
                PdfObject::Name("JBIG2Decode".into()),
            ]),
        );
        let parms = stream.dictionary.remove("DecodeParms").unwrap();
        stream.dictionary.insert(
            "DecodeParms".into(),
            PdfObject::Array(vec![PdfObject::Null, parms]),
        );
        assert_eq!(decode_image(&doc, &stream).unwrap().rgb, image.rgb);
        stream.dictionary.remove("DecodeParms");
        assert!(decode_image(&doc, &stream).is_none());
    }

    #[test]
    fn jbig2_decode_array_inverts_stencil_coverage() {
        let doc = PdfDocument::new_empty("1.7");
        let mut stream = stream(&SYMBOLS[13..], "JBIG2Decode");
        stream
            .dictionary
            .insert("ImageMask".into(), PdfObject::Bool(true));
        let normal = decode_image(&doc, &stream).unwrap();
        stream.dictionary.insert(
            "Decode".into(),
            PdfObject::Array(vec![PdfObject::Integer(1), PdfObject::Integer(0)]),
        );
        let inverted = decode_image(&doc, &stream).unwrap();
        assert!(normal.rgb.is_empty());
        assert!(normal
            .alpha
            .iter()
            .zip(inverted.alpha)
            .all(|(a, b)| u16::from(*a) + u16::from(b) == 255));
    }

    #[test]
    fn codec_headers_cannot_hide_oversized_allocations_behind_pdf_dimensions() {
        let doc = PdfDocument::new_empty("1.7");
        let mut jpx = jpx_codestream(PALETTE).unwrap().to_vec();
        jpx[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_image(&doc, &stream(&jpx, "JPXDecode")).is_none());
        let mut jbig = SYMBOLS[300..].to_vec();
        jbig[11..15].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_image(&doc, &stream(&jbig, "JBIG2Decode")).is_none());
        // An enormous referred-to count must be rejected before the codec's
        // Vec::with_capacity, even when the stream itself is tiny.
        let long_refs = [0, 0, 0, 1, 48, 255, 255, 255, 255];
        assert!(decode_jbig2(&long_refs, None).is_none());
        for length in [0, 1, 8, 13, 22, 49, 100] {
            assert!(decode_image(&doc, &stream(&PALETTE[..length], "JPXDecode")).is_none());
            assert!(
                decode_image(&doc, &stream(&SYMBOLS[13..13 + length], "JBIG2Decode")).is_none()
            );
        }
    }

    #[test]
    fn tiny_jpx_precinct_header_is_rejected_before_packet_buffer_allocation() {
        let mut data = jpx_codestream(PALETTE).unwrap().to_vec();
        for offset in [8, 12, 24, 28] {
            data[offset..offset + 4].copy_from_slice(&600_u32.to_be_bytes());
        }
        // Add explicit 1x1 precincts to the fixture's COD marker. The file
        // stays under 200 bytes and its pixel count is within normal limits,
        // but it requests 360,000 precincts/code blocks for one small tile.
        data.splice(45..59, [255, 82, 0, 13, 1, 0, 0, 1, 0, 0, 4, 4, 0, 1, 0]);
        check_jpx(&data).unwrap();
        let image = hayro_jpeg2000::Image::new(&data, &Default::default()).unwrap();
        let mut context = Default::default();
        assert!(matches!(
            image.decode(&mut context),
            Err(hayro_jpeg2000::DecodeError::Validation(
                hayro_jpeg2000::ValidationError::ImageTooLarge
            ))
        ));
    }

    #[test]
    fn jbig2_symbol_counts_are_bounded_across_global_dictionaries() {
        let mut global = SYMBOLS[13..300].to_vec();
        // Each dictionary declares a permissible 20,000 exports + 20,000
        // new symbols. Together they exceed the per-image metadata budget.
        global[21..25].copy_from_slice(&20_000_u32.to_be_bytes());
        global[25..29].copy_from_slice(&20_000_u32.to_be_bytes());
        let mut second = global.clone();
        second[..4].copy_from_slice(&1_u32.to_be_bytes());
        global.extend(second);
        let (mut segments, mut pixels, mut symbols) = (0, 0, 0);
        assert!(check_jbig2(&global, &mut segments, &mut pixels, &mut symbols).is_none());
        assert!(decode_jbig2(&SYMBOLS[300..], Some(&global)).is_none());
    }
}
