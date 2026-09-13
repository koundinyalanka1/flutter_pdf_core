//! Decoding PDF image XObjects into RGB samples.
//!
//! `PdfDocument::stream_data` already unwinds Flate/ASCII filters, but it
//! stops at `DCTDecode` because that is a picture format rather than a byte
//! filter. This module handles the split: JPEG payloads go to the decoder,
//! everything else comes back as raw component samples that are then read
//! through the image's colour space.

use pdf_core::document::PdfDocument;
use pdf_core::filter::{decode, DecodeParms};
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;

use crate::canvas::Rgb;

/// A decoded image, ready to sample.
pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    /// RGB triples, row-major, top-left origin. Empty for a stencil mask.
    pub rgb: Vec<u8>,
    /// Per-pixel alpha (0-255). Empty means fully opaque.
    pub alpha: Vec<u8>,
    /// True when this is a `/ImageMask` stencil: `alpha` selects where the
    /// current fill colour is painted and `rgb` is unused.
    pub is_stencil: bool,
}

impl DecodedImage {
    pub fn sample(&self, x: usize, y: usize) -> Option<(Rgb, f32)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = y * self.width + x;
        let alpha = if self.alpha.is_empty() {
            1.0
        } else {
            self.alpha[index] as f32 / 255.0
        };
        if self.is_stencil {
            return Some((Rgb::BLACK, alpha));
        }
        let offset = index * 3;
        Some((
            Rgb::new(
                self.rgb[offset] as f32 / 255.0,
                self.rgb[offset + 1] as f32 / 255.0,
                self.rgb[offset + 2] as f32 / 255.0,
            ),
            alpha,
        ))
    }
}

/// Colour spaces an image's samples can be read through.
#[derive(Debug, Clone)]
enum ColorSpace {
    Gray,
    Rgb,
    Cmyk,
    /// Palette: base space plus a lookup table of base-space components.
    Indexed(Box<ColorSpace>, Vec<u8>),
    /// Separation / DeviceN: approximated as ink coverage.
    Tint(usize),
}

impl ColorSpace {
    fn components(&self) -> usize {
        match self {
            ColorSpace::Gray => 1,
            ColorSpace::Rgb => 3,
            ColorSpace::Cmyk => 4,
            ColorSpace::Indexed(..) => 1,
            ColorSpace::Tint(n) => *n,
        }
    }

    /// Convert one pixel's components (already normalised to 0..1, except
    /// Indexed which passes the raw palette index) to RGB.
    fn to_rgb(&self, components: &[f64], raw_index: u32) -> Rgb {
        match self {
            ColorSpace::Gray => Rgb::gray(components.first().copied().unwrap_or(0.0) as f32),
            ColorSpace::Rgb => Rgb::new(
                components.first().copied().unwrap_or(0.0) as f32,
                components.get(1).copied().unwrap_or(0.0) as f32,
                components.get(2).copied().unwrap_or(0.0) as f32,
            ),
            ColorSpace::Cmyk => Rgb::from_cmyk(
                components.first().copied().unwrap_or(0.0) as f32,
                components.get(1).copied().unwrap_or(0.0) as f32,
                components.get(2).copied().unwrap_or(0.0) as f32,
                components.get(3).copied().unwrap_or(0.0) as f32,
            ),
            ColorSpace::Indexed(base, lookup) => {
                let n = base.components();
                let start = raw_index as usize * n;
                if start + n > lookup.len() {
                    return Rgb::BLACK;
                }
                let values: Vec<f64> = lookup[start..start + n]
                    .iter()
                    .map(|&b| b as f64 / 255.0)
                    .collect();
                base.to_rgb(&values, 0)
            }
            // One or more inks; treat maximum coverage as darkness.
            ColorSpace::Tint(_) => {
                let ink = components.iter().cloned().fold(0.0_f64, f64::max);
                Rgb::gray((1.0 - ink) as f32)
            }
        }
    }
}

/// Decode an image XObject. Returns `None` for formats this renderer cannot
/// read (JPXDecode/JBIG2, chiefly) rather than failing the whole page.
pub fn decode_image(doc: &PdfDocument, stream: &PdfStream) -> Option<DecodedImage> {
    let dict = &stream.dictionary;
    let width = dict_int(doc, dict, "Width", "W")? as usize;
    let height = dict_int(doc, dict, "Height", "H")? as usize;
    if width == 0 || height == 0 || width * height > 64_000_000 {
        return None;
    }

    let is_stencil = dict
        .get("ImageMask")
        .or_else(|| dict.get("IM"))
        .map(|o| doc.resolve_value(o))
        .map(|o| matches!(o, PdfObject::Bool(true)))
        .unwrap_or(false);

    let bpc = if is_stencil {
        1
    } else {
        dict_int(doc, dict, "BitsPerComponent", "BPC").unwrap_or(8) as usize
    };

    let decode_array: Option<Vec<f64>> = dict
        .get("Decode")
        .or_else(|| dict.get("D"))
        .map(|o| doc.resolve_value(o))
        .and_then(|o| match o {
            PdfObject::Array(items) => Some(items.iter().filter_map(as_f64).collect()),
            _ => None,
        });

    let filters = filter_names(doc, dict);
    let data = defilter(doc, stream, &filters)?;

    let mut image = if filters.iter().any(|f| f == "DCTDecode" || f == "DCT") {
        decode_jpeg(&data, width, height)?
    } else {
        let space = if is_stencil {
            ColorSpace::Gray
        } else {
            color_space(doc, dict)?
        };
        decode_raw(&data, width, height, bpc, &space, decode_array.as_deref(), is_stencil)?
    };

    if is_stencil {
        image.is_stencil = true;
    } else {
        apply_soft_mask(doc, dict, &mut image);
    }
    Some(image)
}

/// Apply the whole filter chain except a trailing image codec, which is
/// handed back untouched for the JPEG decoder.
fn defilter(doc: &PdfDocument, stream: &PdfStream, filters: &[String]) -> Option<Vec<u8>> {
    let mut data = stream.data.clone();
    let parms = decode_parms(doc, &stream.dictionary, filters.len());
    for (i, name) in filters.iter().enumerate() {
        match name.as_str() {
            // Image codecs terminate the byte-filter chain.
            "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" | "CCITTFaxDecode" | "CCF" => {
                if name == "DCTDecode" || name == "DCT" {
                    return Some(data);
                }
                return None; // JPX/JBIG2/CCITT are out of scope.
            }
            _ => {
                data = decode(name, &data, &parms[i]).ok()?;
            }
        }
    }
    Some(data)
}

/// Per-filter `/DecodeParms`, resolved.
///
/// Images are routinely stored as Flate with a PNG `/Predictor`, and without
/// these parameters the predictor is never undone: every row keeps its filter
/// -type byte and stays differenced against the row above. The result is not a
/// subtle artefact — each row shifts one byte further than the last, so the
/// picture shears diagonally and the RGB triples fall out of step into green
/// and magenta fringes. Passing `DecodeParms::default()` here silently did
/// exactly that to every predicted image.
///
/// `/DecodeParms` (and its entries, and their values) may each be indirect, so
/// everything is resolved through the document rather than read off the
/// dictionary directly.
fn decode_parms(doc: &PdfDocument, dict: &Dictionary, count: usize) -> Vec<DecodeParms> {
    let mut out = vec![DecodeParms::default(); count.max(1)];
    let raw = dict
        .get("DecodeParms")
        .or_else(|| dict.get("DP"))
        .map(|o| doc.resolve_value(o));
    match raw {
        Some(PdfObject::Dictionary(d)) => {
            out[0] = parms_from_dict(doc, &d);
        }
        Some(PdfObject::Array(items)) => {
            for (i, item) in items.iter().take(out.len()).enumerate() {
                if let PdfObject::Dictionary(d) = doc.resolve_value(item) {
                    out[i] = parms_from_dict(doc, &d);
                }
            }
        }
        _ => {}
    }
    out
}

fn parms_from_dict(doc: &PdfDocument, dict: &Dictionary) -> DecodeParms {
    let get = |key: &str, default: i64| -> i64 {
        dict.get(key)
            .map(|o| doc.resolve_value(o))
            .and_then(|o| o.as_i64())
            .unwrap_or(default)
    };
    DecodeParms {
        predictor: get("Predictor", 1).clamp(1, 15) as u8,
        colors: get("Colors", 1).max(1) as usize,
        bits_per_component: get("BitsPerComponent", 8).max(1) as usize,
        columns: get("Columns", 1).max(1) as usize,
    }
}

fn decode_jpeg(data: &[u8], width: usize, height: usize) -> Option<DecodedImage> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let w = info.width as usize;
    let h = info.height as usize;

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => {
            pixels.iter().flat_map(|&v| [v, v, v]).collect()
        }
        jpeg_decoder::PixelFormat::L16 => pixels
            .chunks_exact(2)
            .flat_map(|c| {
                let v = c[1]; // take the high byte
                [v, v, v]
            })
            .collect(),
        jpeg_decoder::PixelFormat::CMYK32 => pixels
            .chunks_exact(4)
            .flat_map(|c| {
                // jpeg-decoder hands back Adobe-inverted CMYK.
                let rgb = Rgb::from_cmyk(
                    1.0 - c[0] as f32 / 255.0,
                    1.0 - c[1] as f32 / 255.0,
                    1.0 - c[2] as f32 / 255.0,
                    1.0 - c[3] as f32 / 255.0,
                );
                [
                    (rgb.r * 255.0) as u8,
                    (rgb.g * 255.0) as u8,
                    (rgb.b * 255.0) as u8,
                ]
            })
            .collect(),
    };

    // Trust the JPEG's own dimensions over the dictionary's if they disagree.
    let _ = (width, height);
    Some(DecodedImage {
        width: w,
        height: h,
        rgb,
        alpha: Vec::new(),
        is_stencil: false,
    })
}

fn decode_raw(
    data: &[u8],
    width: usize,
    height: usize,
    bpc: usize,
    space: &ColorSpace,
    decode_array: Option<&[f64]>,
    is_stencil: bool,
) -> Option<DecodedImage> {
    let components = space.components();
    let max_value = ((1u32 << bpc.min(16)) - 1) as f64;
    // PDF image rows are padded to byte boundaries.
    let row_bits = width * components * bpc;
    let row_bytes = row_bits.div_ceil(8);
    if data.len() < row_bytes * height {
        // Truncated image data is common in damaged files; render what we can
        // rather than dropping the page.
        if data.len() < row_bytes {
            return None;
        }
    }

    let mut rgb = if is_stencil {
        Vec::new()
    } else {
        Vec::with_capacity(width * height * 3)
    };
    let mut alpha = if is_stencil {
        Vec::with_capacity(width * height)
    } else {
        Vec::new()
    };

    let mut pixel = vec![0.0f64; components];
    for y in 0..height {
        let row_start = y * row_bytes;
        for x in 0..width {
            let mut raw_first = 0u32;
            for c in 0..components {
                let bit = (x * components + c) * bpc;
                let raw = read_bits(data, row_start * 8 + bit, bpc).unwrap_or(0);
                if c == 0 {
                    raw_first = raw;
                }
                let mut value = raw as f64 / max_value;
                // /Decode remaps each component's range.
                if let Some(array) = decode_array {
                    if array.len() >= (c + 1) * 2 {
                        let (dmin, dmax) = (array[c * 2], array[c * 2 + 1]);
                        value = dmin + value * (dmax - dmin);
                    }
                }
                pixel[c] = value;
            }

            if is_stencil {
                // Sample 0 paints, 1 leaves the background — unless /Decode
                // flipped it, which the remap above has already applied.
                alpha.push(if pixel[0] < 0.5 { 255 } else { 0 });
            } else {
                // Indexed needs the raw palette index, post-/Decode.
                let index = match space {
                    ColorSpace::Indexed(..) => (pixel[0] * max_value).round().max(0.0) as u32,
                    _ => raw_first,
                };
                let color = space.to_rgb(&pixel, index);
                rgb.push((color.r.clamp(0.0, 1.0) * 255.0) as u8);
                rgb.push((color.g.clamp(0.0, 1.0) * 255.0) as u8);
                rgb.push((color.b.clamp(0.0, 1.0) * 255.0) as u8);
            }
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

/// Blend an `/SMask` grayscale image into the alpha channel.
fn apply_soft_mask(doc: &PdfDocument, dict: &Dictionary, image: &mut DecodedImage) {
    let Some(entry) = dict.get("SMask") else {
        return;
    };
    let PdfObject::Stream(mask_stream) = doc.resolve_value(entry) else {
        return;
    };
    let Some(mask) = decode_image(doc, &mask_stream) else {
        return;
    };
    if mask.rgb.is_empty() {
        return;
    }

    let mut alpha = Vec::with_capacity(image.width * image.height);
    for y in 0..image.height {
        // Soft masks may be a different resolution than the image.
        let my = y * mask.height / image.height.max(1);
        for x in 0..image.width {
            let mx = x * mask.width / image.width.max(1);
            let offset = (my.min(mask.height - 1) * mask.width + mx.min(mask.width - 1)) * 3;
            alpha.push(mask.rgb.get(offset).copied().unwrap_or(255));
        }
    }
    image.alpha = alpha;
}

fn color_space(doc: &PdfDocument, dict: &Dictionary) -> Option<ColorSpace> {
    let entry = dict.get("ColorSpace").or_else(|| dict.get("CS"))?;
    parse_color_space(doc, &doc.resolve_value(entry))
}

fn parse_color_space(doc: &PdfDocument, object: &PdfObject) -> Option<ColorSpace> {
    match object {
        PdfObject::Name(name) => Some(match name.as_str() {
            "DeviceGray" | "G" | "CalGray" => ColorSpace::Gray,
            "DeviceCMYK" | "CMYK" => ColorSpace::Cmyk,
            _ => ColorSpace::Rgb,
        }),
        PdfObject::Array(items) => {
            let family = items.first().and_then(PdfObject::as_name)?;
            match family {
                "ICCBased" => {
                    let stream = doc.resolve_value(items.get(1)?);
                    let n = match &stream {
                        PdfObject::Stream(s) => s
                            .dictionary
                            .get("N")
                            .and_then(PdfObject::as_i64)
                            .unwrap_or(3),
                        _ => 3,
                    };
                    Some(match n {
                        1 => ColorSpace::Gray,
                        4 => ColorSpace::Cmyk,
                        _ => ColorSpace::Rgb,
                    })
                }
                "Indexed" | "I" => {
                    let base = parse_color_space(doc, &doc.resolve_value(items.get(1)?))?;
                    let lookup = match doc.resolve_value(items.get(3)?) {
                        PdfObject::Stream(s) => doc.stream_data(&s).ok()?,
                        PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => bytes,
                        _ => return None,
                    };
                    Some(ColorSpace::Indexed(Box::new(base), lookup))
                }
                "CalRGB" | "Lab" => Some(ColorSpace::Rgb),
                "CalGray" => Some(ColorSpace::Gray),
                "Separation" => Some(ColorSpace::Tint(1)),
                "DeviceN" => {
                    let n = match doc.resolve_value(items.get(1)?) {
                        PdfObject::Array(names) => names.len(),
                        _ => 1,
                    };
                    Some(ColorSpace::Tint(n))
                }
                "DeviceGray" => Some(ColorSpace::Gray),
                "DeviceCMYK" => Some(ColorSpace::Cmyk),
                _ => Some(ColorSpace::Rgb),
            }
        }
        _ => None,
    }
}

fn filter_names(doc: &PdfDocument, dict: &Dictionary) -> Vec<String> {
    let Some(entry) = dict.get("Filter").or_else(|| dict.get("F")) else {
        return Vec::new();
    };
    match doc.resolve_value(entry) {
        PdfObject::Name(name) => vec![name],
        PdfObject::Array(items) => items
            .iter()
            .filter_map(|o| o.as_name().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Read `count` bits starting at `bit_offset`, MSB first.
fn read_bits(data: &[u8], bit_offset: usize, count: usize) -> Option<u32> {
    if count == 8 {
        return data.get(bit_offset / 8).map(|&b| b as u32);
    }
    if count == 16 {
        let i = bit_offset / 8;
        return Some(u32::from(*data.get(i)?) << 8 | u32::from(*data.get(i + 1)?));
    }
    let mut value = 0u32;
    for i in 0..count {
        let bit = bit_offset + i;
        let byte = *data.get(bit / 8)?;
        let set = (byte >> (7 - (bit % 8))) & 1;
        value = (value << 1) | set as u32;
    }
    Some(value)
}

fn dict_int(doc: &PdfDocument, dict: &Dictionary, key: &str, abbrev: &str) -> Option<i64> {
    let entry = dict.get(key).or_else(|| dict.get(abbrev))?;
    doc.resolve_value(entry).as_i64()
}

fn as_f64(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(v) => Some(*v as f64),
        PdfObject::Real(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_packed_bits() {
        // 0b1010_1100
        let data = [0xACu8];
        assert_eq!(read_bits(&data, 0, 1), Some(1));
        assert_eq!(read_bits(&data, 1, 1), Some(0));
        assert_eq!(read_bits(&data, 0, 4), Some(0b1010));
        assert_eq!(read_bits(&data, 4, 4), Some(0b1100));
        assert_eq!(read_bits(&data, 0, 8), Some(0xAC));
    }

    #[test]
    fn gray_ramp_decodes_to_gray_pixels() {
        let space = ColorSpace::Gray;
        let data = vec![0x00, 0x80, 0xFF];
        let image = decode_raw(&data, 3, 1, 8, &space, None, false).unwrap();
        assert_eq!(&image.rgb[0..3], &[0, 0, 0]);
        assert_eq!(&image.rgb[6..9], &[255, 255, 255]);
    }

    #[test]
    fn indexed_palette_maps_through_the_lookup_table() {
        // Two-entry RGB palette: red then green.
        let space = ColorSpace::Indexed(
            Box::new(ColorSpace::Rgb),
            vec![255, 0, 0, 0, 255, 0],
        );
        // 1 bit per pixel: 0, 1 -> red, green (packed into one byte).
        let image = decode_raw(&[0b0100_0000], 2, 1, 1, &space, None, false).unwrap();
        assert_eq!(&image.rgb[0..3], &[255, 0, 0]);
        assert_eq!(&image.rgb[3..6], &[0, 255, 0]);
    }

    #[test]
    fn stencil_mask_paints_where_the_bit_is_zero() {
        let image = decode_raw(
            &[0b0100_0000],
            2,
            1,
            1,
            &ColorSpace::Gray,
            None,
            true,
        )
        .unwrap();
        assert!(image.is_stencil);
        assert_eq!(image.alpha, vec![255, 0]);
    }

    #[test]
    fn decode_array_inverts_a_stencil() {
        let image = decode_raw(
            &[0b0100_0000],
            2,
            1,
            1,
            &ColorSpace::Gray,
            Some(&[1.0, 0.0]),
            true,
        )
        .unwrap();
        assert_eq!(image.alpha, vec![0, 255]);
    }

    #[test]
    fn cmyk_samples_convert_to_rgb() {
        // Pure cyan: C=1 M=0 Y=0 K=0.
        let image =
            decode_raw(&[255, 0, 0, 0], 1, 1, 8, &ColorSpace::Cmyk, None, false).unwrap();
        assert_eq!(&image.rgb[0..3], &[0, 255, 255]);
    }

    #[test]
    fn rejects_absurd_dimensions() {
        let mut dict = Dictionary::new();
        dict.insert("Width".into(), PdfObject::Integer(100_000));
        dict.insert("Height".into(), PdfObject::Integer(100_000));
        let doc = PdfDocument::new_empty("1.7");
        let stream = PdfStream::new(dict, Vec::new());
        assert!(decode_image(&doc, &stream).is_none());
    }
}
