//! Decoding PDF image XObjects into RGB samples.
//!
//! `PdfDocument::stream_data` already unwinds the byte filters (Flate, LZW,
//! RunLength, ASCII), but stops at the picture formats. This module handles
//! the split: JPEG payloads go to the JPEG decoder, CCITT G3/G4 fax data to
//! [`crate::ccitt`], and everything else comes back as raw component samples
//! read through the image's colour space.
//!
//! JPEG 2000 and JBIG2 use standalone Rust codecs with resource checks before
//! decoding. Embedded JBIG2 symbol dictionaries are resolved via JBIG2Globals.

use pdf_core::document::PdfDocument;
use pdf_core::filter::{decode, DecodeParms};
use pdf_core::object::{Dictionary, PdfObject};
use pdf_core::stream::PdfStream;

use crate::canvas::Rgb;
use crate::ccitt;

#[path = "image_codecs.rs"]
mod codecs;

const MAX_IMAGE_PIXELS: usize = 64_000_000;
const MAX_IMAGE_DEPTH: usize = 8;
const MAX_COLOR_SPACE_DEPTH: usize = 16;
const MAX_COLOR_COMPONENTS: usize = 32;

fn image_pixel_count(width: usize, height: usize) -> Option<usize> {
    let count = width.checked_mul(height)?;
    (count > 0 && count <= MAX_IMAGE_PIXELS).then_some(count)
}

/// A decoded image, ready to sample.
pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    /// RGB triples, row-major, top-left origin — or, when [`Self::gray`] is
    /// set, one gray sample a pixel. Empty for a stencil mask.
    pub rgb: Vec<u8>,
    /// The samples are gray, one byte a pixel. A scanned book is gray or
    /// black and white; held as RGB, a page's scan took three times the
    /// memory — 70 MB rather than 23 for an Internet Archive page — and too
    /// much to keep between renders.
    pub gray: bool,
    /// Per-pixel alpha (0-255). Empty means fully opaque.
    pub alpha: Vec<u8>,
    /// True when this is a `/ImageMask` stencil: `alpha` selects where the
    /// current fill colour is painted and `rgb` is unused.
    pub is_stencil: bool,
    /// Coverage from a `/Mask` or `/SMask` image of another size, kept at its
    /// own resolution. A scanned book's text mask is often three times as
    /// fine as the colour it reveals; squeezed onto the colour's grid, every
    /// letter would blur. Applies on top of `alpha`.
    pub mask: Option<AlphaPlane>,
}

impl DecodedImage {
    /// Bytes a pixel takes in `rgb`.
    pub fn channels(&self) -> usize {
        if self.gray {
            1
        } else {
            3
        }
    }

    /// Memory held by the decoded samples.
    pub fn byte_size(&self) -> usize {
        self.rgb.len() + self.alpha.len() + self.mask.as_ref().map_or(0, |mask| mask.data.len())
    }
}

/// Per-pixel coverage, 0-255, row-major from the top left.
pub struct AlphaPlane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl DecodedImage {
    pub fn sample(&self, x: usize, y: usize) -> Option<(Rgb, f32)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = y * self.width + x;
        let mut alpha = if self.alpha.is_empty() {
            1.0
        } else {
            self.alpha[index] as f32 / 255.0
        };
        if let Some(mask) = &self.mask {
            let mx = (x * mask.width / self.width).min(mask.width - 1);
            let my = (y * mask.height / self.height).min(mask.height - 1);
            alpha *= f32::from(mask.data[my * mask.width + mx]) / 255.0;
        }
        if self.is_stencil {
            return Some((Rgb::BLACK, alpha));
        }
        if self.gray {
            return Some((Rgb::gray(f32::from(self.rgb[index]) / 255.0), alpha));
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
                let Some(start) = (raw_index as usize).checked_mul(n) else {
                    return Rgb::BLACK;
                };
                let Some(values) = lookup.get(start..).and_then(|tail| tail.get(..n)) else {
                    return Rgb::BLACK;
                };
                let values: Vec<f64> = values.iter().map(|&b| b as f64 / 255.0).collect();
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

/// Decode an image XObject. Returns `None` for damaged or resource-limited
/// images rather than failing the whole page.
/// Images are limited to 64 million pixels, 32 colour components and 16 levels
/// of colour-space nesting. Cyclic masks and mask chains beyond eight images
/// are ignored while preserving the base image.
pub fn decode_image(doc: &PdfDocument, stream: &PdfStream) -> Option<DecodedImage> {
    decode_image_inner(doc, stream, &[])
}

fn decode_image_inner(
    doc: &PdfDocument,
    stream: &PdfStream,
    ancestors: &[&PdfStream],
) -> Option<DecodedImage> {
    if ancestors.len() >= MAX_IMAGE_DEPTH
        || ancestors
            .iter()
            .any(|ancestor| std::ptr::eq(*ancestor, stream))
    {
        return None;
    }
    let mut ancestors = ancestors.to_vec();
    ancestors.push(stream);
    let dict = &stream.dictionary;
    let width = usize::try_from(dict_int(doc, dict, "Width", "W")?).ok()?;
    let height = usize::try_from(dict_int(doc, dict, "Height", "H")?).ok()?;
    image_pixel_count(width, height)?;

    let is_stencil = dict
        .get("ImageMask")
        .or_else(|| dict.get("IM"))
        .map(|o| doc.resolve_value(o))
        .map(|o| matches!(o, PdfObject::Bool(true)))
        .unwrap_or(false);

    let bpc = if is_stencil {
        1
    } else {
        usize::try_from(dict_int(doc, dict, "BitsPerComponent", "BPC").unwrap_or(8)).ok()?
    };
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }

    let decode_array: Option<Vec<f64>> = dict
        .get("Decode")
        .or_else(|| dict.get("D"))
        .map(|o| doc.resolve_value(o))
        .and_then(|o| match o {
            PdfObject::Array(items) => Some(items.iter().filter_map(as_f64).collect()),
            _ => None,
        });

    // `/Mask` as an array is colour-key masking, applied while decoding
    // because it compares raw samples; as a stream it is applied below.
    let color_key: Option<Vec<u32>> = match dict.get("Mask").map(|o| doc.resolve_value(o)) {
        Some(PdfObject::Array(items)) if !is_stencil && !items.is_empty() => items
            .iter()
            .map(|v| u32::try_from(doc.resolve_value(v).as_i64()?).ok())
            .collect(),
        _ => None,
    };

    let filters = filter_names(doc, dict);
    let data = defilter(doc, stream, &filters, width, height)?;

    let mut image = if filters.iter().any(|f| f == "DCTDecode" || f == "DCT") {
        decode_jpeg(&data, width, height, color_key.as_deref())?
    } else if filters.iter().any(|f| f == "JPXDecode") {
        codecs::decode_jpx(doc, dict, &data, is_stencil, decode_array.as_deref())?
    } else if let Some(index) = filters.iter().position(|f| f == "JBIG2Decode") {
        let parms = raw_decode_parms(doc, dict, filters.len());
        let globals = parms[index].as_ref().and_then(|p| p.get("JBIG2Globals"));
        let globals = match globals {
            Some(object) => {
                let PdfObject::Stream(stream) = resolve_object(doc, object)? else {
                    return None;
                };
                if stream.data.len() > codecs::MAX_CODEC_BYTES {
                    return None;
                }
                Some(doc.stream_data(stream).ok()?)
            }
            None => None,
        };
        let (samples, w, h) = codecs::decode_jbig2(&data, globals.as_deref())?;
        decode_raw(
            &samples,
            w,
            h,
            1,
            &ColorSpace::Gray,
            decode_array.as_deref(),
            is_stencil,
            color_key.as_deref(),
        )?
    } else {
        let space = if is_stencil {
            ColorSpace::Gray
        } else {
            color_space(doc, dict)?
        };
        decode_raw(
            &data,
            width,
            height,
            bpc,
            &space,
            decode_array.as_deref(),
            is_stencil,
            color_key.as_deref(),
        )?
    };

    if is_stencil {
        image.is_stencil = true;
    } else {
        apply_masks(doc, dict, &mut image, &ancestors);
    }
    Some(image)
}

/// Apply the whole filter chain except a trailing image codec, which is
/// handed back untouched for the JPEG decoder.
fn defilter(
    doc: &PdfDocument,
    stream: &PdfStream,
    filters: &[String],
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let mut data = stream.data.clone();
    let parms = decode_parms(doc, &stream.dictionary, filters.len());
    let raw_parms = raw_decode_parms(doc, &stream.dictionary, filters.len());
    for (i, name) in filters.iter().enumerate() {
        match name.as_str() {
            // JPEG is decoded by the image path rather than here, so its
            // bytes pass through untouched.
            "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" => return Some(data),
            "CCITTFaxDecode" | "CCF" => {
                let params = ccitt_params(
                    doc,
                    raw_parms.get(i).and_then(Option::as_ref),
                    width,
                    height,
                )?;
                return ccitt::decode(&data, &params);
            }
            _ => {
                data = decode(name, &data, &parms[i]).ok()?;
            }
        }
    }
    Some(data)
}

/// The per-filter `/DecodeParms` dictionaries themselves, which CCITT needs
/// because its parameters do not fit the byte-filter `DecodeParms` struct.
fn raw_decode_parms(doc: &PdfDocument, dict: &Dictionary, count: usize) -> Vec<Option<Dictionary>> {
    let mut out = vec![None; count.max(1)];
    let raw = dict
        .get("DecodeParms")
        .or_else(|| dict.get("DP"))
        .map(|o| doc.resolve_value(o));
    match raw {
        Some(PdfObject::Dictionary(d)) => out[0] = Some(d),
        Some(PdfObject::Array(items)) => {
            for (i, item) in items.iter().take(out.len()).enumerate() {
                if let PdfObject::Dictionary(d) = doc.resolve_value(item) {
                    out[i] = Some(d);
                }
            }
        }
        _ => {}
    }
    out
}

fn ccitt_params(
    doc: &PdfDocument,
    parms: Option<&Dictionary>,
    width: usize,
    height: usize,
) -> Option<ccitt::CcittParams> {
    let get = |key: &str, default: i64| -> i64 {
        parms
            .and_then(|d| d.get(key))
            .map(|o| doc.resolve_value(o))
            .and_then(|o| o.as_i64())
            .unwrap_or(default)
    };
    let flag = |key: &str| -> bool {
        matches!(
            parms.and_then(|d| d.get(key)).map(|o| doc.resolve_value(o)),
            Some(PdfObject::Bool(true))
        )
    };
    let columns = usize::try_from(get("Columns", width as i64)).ok()?;
    // The fax decoder supports rows up to 65536 pixels wide. A Rows value
    // larger than the image height only creates unused trailing padding;
    // Rows = 0 means decode as many rows as this image actually needs.
    if !(1..=65536).contains(&columns) {
        return None;
    }
    let requested_rows = usize::try_from(get("Rows", height as i64)).ok()?;
    let rows = if requested_rows == 0 {
        height
    } else {
        requested_rows.min(height)
    };
    image_pixel_count(columns, rows)?;
    Some(ccitt::CcittParams {
        k: get("K", 0).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        // The specification's default is 1728, but the image dictionary's
        // /Width is what the rest of the pipeline will read rows against, and
        // the two only ever differ in broken files. Preferring /Width is
        // never worse and rescues streams that simply omitted /Columns.
        columns,
        rows,
        black_is_1: flag("BlackIs1"),
        byte_align: flag("EncodedByteAlign"),
    })
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
        early_change: get("EarlyChange", 1) != 0,
    }
}

fn decode_jpeg(
    data: &[u8],
    width: usize,
    height: usize,
    color_key: Option<&[u32]>,
) -> Option<DecodedImage> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.set_max_decoding_buffer_size(MAX_IMAGE_PIXELS * 4);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    let w = info.width as usize;
    let h = info.height as usize;
    // Check the JPEG's actual dimensions before allocating its pixel buffer;
    // the enclosing PDF dictionary can lie about both width and height.
    image_pixel_count(w, h)?;
    let pixels = decoder.decode().ok()?;

    let gray = matches!(
        info.pixel_format,
        jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::L16
    );
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8 => pixels,
        // The high byte of each sample.
        jpeg_decoder::PixelFormat::L16 => pixels.chunks_exact(2).map(|c| c[1]).collect(),
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
    // Colour-key masking compares the samples as stored, which for a baseline
    // JPEG are its 8-bit components.
    let alpha = match (color_key, info.pixel_format) {
        (Some(ranges), jpeg_decoder::PixelFormat::L8) => key_alpha(&rgb, 1, ranges),
        (Some(ranges), jpeg_decoder::PixelFormat::RGB24) => key_alpha(&rgb, 3, ranges),
        _ => Vec::new(),
    };
    Some(DecodedImage {
        width: w,
        height: h,
        rgb,
        gray,
        alpha,
        is_stencil: false,
        mask: None,
    })
}

/// Alpha for colour-key masking: transparent where every component of a
/// pixel (`components` bytes) lies in its `[min, max]` range.
fn key_alpha(samples: &[u8], components: usize, ranges: &[u32]) -> Vec<u8> {
    if ranges.len() < components * 2 {
        return Vec::new();
    }
    samples
        .chunks_exact(components)
        .map(|pixel| {
            let keyed = pixel
                .iter()
                .enumerate()
                .all(|(c, &v)| (ranges[c * 2]..=ranges[c * 2 + 1]).contains(&u32::from(v)));
            if keyed {
                0
            } else {
                255
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn decode_raw(
    data: &[u8],
    width: usize,
    height: usize,
    bpc: usize,
    space: &ColorSpace,
    decode_array: Option<&[f64]>,
    is_stencil: bool,
    color_key: Option<&[u32]>,
) -> Option<DecodedImage> {
    let pixel_count = image_pixel_count(width, height)?;
    let components = space.components();
    if !(1..=MAX_COLOR_COMPONENTS).contains(&components) || !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    let max_value = ((1u32 << bpc.min(16)) - 1) as f64;
    // PDF image rows are padded to byte boundaries.
    let row_bits = width.checked_mul(components)?.checked_mul(bpc)?;
    let row_bytes = row_bits.div_ceil(8);
    let total_bytes = row_bytes.checked_mul(height)?;
    total_bytes.checked_mul(8)?;
    if data.len() < total_bytes {
        // Truncated image data is common in damaged files; render what we can
        // rather than dropping the page.
        if data.len() < row_bytes {
            return None;
        }
    }

    // Colour-key masking (`/Mask` as `[min max ...]`): a pixel whose raw
    // samples all fall in their ranges is transparent (ISO 32000-1, 8.9.6.4).
    let key = color_key.filter(|ranges| !is_stencil && ranges.len() >= components * 2);
    let gray = !is_stencil && matches!(space, ColorSpace::Gray);
    let mut rgb = if is_stencil {
        Vec::new()
    } else {
        Vec::with_capacity(pixel_count.checked_mul(if gray { 1 } else { 3 })?)
    };
    let mut alpha = if is_stencil || key.is_some() {
        Vec::with_capacity(pixel_count)
    } else {
        Vec::new()
    };

    let mut pixel = vec![0.0f64; components];
    for y in 0..height {
        let row_start = y * row_bytes;
        for x in 0..width {
            let mut raw_first = 0u32;
            let mut keyed = key.is_some();
            for c in 0..components {
                let bit = (x * components + c) * bpc;
                let raw = read_bits(data, row_start * 8 + bit, bpc).unwrap_or(0);
                if c == 0 {
                    raw_first = raw;
                }
                if let Some(ranges) = key {
                    keyed &= (ranges[c * 2]..=ranges[c * 2 + 1]).contains(&raw);
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
                if !gray {
                    rgb.push((color.g.clamp(0.0, 1.0) * 255.0) as u8);
                    rgb.push((color.b.clamp(0.0, 1.0) * 255.0) as u8);
                }
                if key.is_some() {
                    alpha.push(if keyed { 0 } else { 255 });
                }
            }
        }
    }

    Some(DecodedImage {
        width,
        height,
        rgb,
        gray,
        alpha,
        is_stencil,
        mask: None,
    })
}

/// Blend an `/SMask` grayscale image into the alpha channel.
/// Apply the image's mask: an `/SMask` (grey coverage), or else a `/Mask`
/// image — an image mask whose painted samples show the image (explicit
/// masking, ISO 32000-1 8.9.6.3). A colour-key `/Mask` array was applied
/// while decoding.
///
/// Explicit masks were ignored, so scanned books built as a coloured layer
/// shown through a sharp text mask — every Internet Archive scan — painted
/// the whole layer: a grey page streaked with the smeared colour that layer
/// carries between the letters.
fn apply_masks(
    doc: &PdfDocument,
    dict: &Dictionary,
    image: &mut DecodedImage,
    ancestors: &[&PdfStream],
) {
    let decode_mask = |entry: &PdfObject| -> Option<DecodedImage> {
        let PdfObject::Stream(mask_stream) = resolve_object(doc, entry)? else {
            return None;
        };
        decode_image_inner(doc, mask_stream, ancestors)
    };
    // A soft mask overrides /Mask (ISO 32000-1, Table 89).
    if let Some(entry) = dict.get("SMask") {
        if let Some(mask) = decode_mask(entry).filter(|m| !m.rgb.is_empty()) {
            let coverage = mask.rgb.iter().step_by(mask.channels()).copied().collect();
            attach_mask(image, mask.width, mask.height, coverage);
        }
        return;
    }
    if let Some(mask) = dict.get("Mask").and_then(decode_mask) {
        let coverage = if mask.is_stencil {
            // Already 255 where the mask paints, which is where the image shows.
            mask.alpha
        } else {
            // Not marked /ImageMask, as the spec requires: read it as one.
            mask.rgb
                .iter()
                .step_by(mask.channels())
                .map(|&gray| 255 - gray)
                .collect()
        };
        attach_mask(image, mask.width, mask.height, coverage);
    }
}

/// Combine `coverage` (`width` x `height`) with the image's own alpha. One the
/// image's size merges into it; any other size stays at its own resolution
/// and is sampled alongside the image when drawn.
fn attach_mask(image: &mut DecodedImage, width: usize, height: usize, coverage: Vec<u8>) {
    if width == 0 || height == 0 || coverage.len() != width * height {
        return;
    }
    if width == image.width && height == image.height {
        if image.alpha.len() == coverage.len() {
            for (a, m) in image.alpha.iter_mut().zip(&coverage) {
                *a = ((u16::from(*a) * u16::from(*m) + 127) / 255) as u8;
            }
        } else {
            image.alpha = coverage;
        }
    } else {
        image.mask = Some(AlphaPlane {
            width,
            height,
            data: coverage,
        });
    }
}

fn color_space(doc: &PdfDocument, dict: &Dictionary) -> Option<ColorSpace> {
    let entry = dict.get("ColorSpace").or_else(|| dict.get("CS"))?;
    parse_color_space(doc, entry, 0)
}

// Borrow reference targets: cloning streams or nested arrays at every step
// turns cyclic masks and colour spaces into unnecessary large allocations.
fn resolve_object<'a>(doc: &'a PdfDocument, object: &'a PdfObject) -> Option<&'a PdfObject> {
    match object {
        PdfObject::Reference(id) => doc.resolve(*id),
        other => Some(other),
    }
}

fn parse_color_space(doc: &PdfDocument, object: &PdfObject, depth: usize) -> Option<ColorSpace> {
    if depth >= MAX_COLOR_SPACE_DEPTH {
        return None;
    }
    match resolve_object(doc, object)? {
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
                    let base = parse_color_space(doc, items.get(1)?, depth + 1)?;
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
                    if !(1..=MAX_COLOR_COMPONENTS).contains(&n) {
                        return None;
                    }
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

    fn gray_stream(value: u8) -> PdfStream {
        let mut dict = Dictionary::new();
        dict.insert("Width".into(), PdfObject::Integer(1));
        dict.insert("Height".into(), PdfObject::Integer(1));
        dict.insert("ColorSpace".into(), PdfObject::Name("DeviceGray".into()));
        PdfStream::new(dict, vec![value])
    }

    #[test]
    fn fax_row_metadata_cannot_allocate_padding_beyond_the_image() {
        let doc = PdfDocument::new_empty("1.7");
        let mut stream = gray_stream(255);
        stream
            .dictionary
            .insert("Filter".into(), PdfObject::Name("CCITTFaxDecode".into()));
        stream
            .dictionary
            .insert("BitsPerComponent".into(), PdfObject::Integer(1));
        let mut parms = Dictionary::new();
        parms.insert("K".into(), PdfObject::Integer(-1));
        parms.insert("Rows".into(), PdfObject::Integer(i64::MAX));
        stream
            .dictionary
            .insert("DecodeParms".into(), PdfObject::Dictionary(parms.clone()));
        let image = decode_image(&doc, &stream).unwrap();
        assert_eq!(image.rgb, [255]);

        parms.insert("Rows".into(), PdfObject::Integer(0));
        assert_eq!(ccitt_params(&doc, Some(&parms), 1, 1).unwrap().rows, 1);
        parms.insert("Columns".into(), PdfObject::Integer(i64::MAX));
        assert!(ccitt_params(&doc, Some(&parms), 1, 1).is_none());
        parms.insert("Columns".into(), PdfObject::Integer(65_536));
        assert!(ccitt_params(&doc, Some(&parms), 1, MAX_IMAGE_PIXELS).is_none());
    }

    #[test]
    fn soft_mask_reference_cycles_preserve_the_base_image() {
        let mut doc = PdfDocument::new_empty("1.7");
        let id = doc.add_object(PdfObject::Null);
        let mut stream = gray_stream(80);
        stream
            .dictionary
            .insert("SMask".into(), PdfObject::Reference(id));
        doc.set_object(id, PdfObject::Stream(stream));
        let PdfObject::Stream(stream) = doc.resolve(id).unwrap() else {
            panic!()
        };
        let image = decode_image(&doc, stream).unwrap();
        assert_eq!(image.rgb, [80]);
        assert!(image.alpha.is_empty());
    }

    #[test]
    fn mutual_soft_mask_cycles_and_deep_chains_are_bounded() {
        let mut doc = PdfDocument::new_empty("1.7");
        let first = doc.add_object(PdfObject::Null);
        let second = doc.add_object(PdfObject::Null);
        let mut stream = gray_stream(80);
        stream
            .dictionary
            .insert("SMask".into(), PdfObject::Reference(second));
        doc.set_object(first, PdfObject::Stream(stream));
        let mut mask = gray_stream(160);
        mask.dictionary
            .insert("SMask".into(), PdfObject::Reference(first));
        doc.set_object(second, PdfObject::Stream(mask));
        let PdfObject::Stream(stream) = doc.resolve(first).unwrap() else {
            panic!()
        };
        assert_eq!(decode_image(&doc, stream).unwrap().alpha, [160]);

        let mut next = first;
        for _ in 0..256 {
            let mut stream = gray_stream(40);
            stream
                .dictionary
                .insert("SMask".into(), PdfObject::Reference(next));
            next = doc.add_object(PdfObject::Stream(stream));
        }
        let PdfObject::Stream(stream) = doc.resolve(next).unwrap() else {
            panic!()
        };
        assert_eq!(decode_image(&doc, stream).unwrap().alpha, [40]);
    }

    #[test]
    fn indexed_color_space_cycles_and_excessive_nesting_are_rejected() {
        let mut doc = PdfDocument::new_empty("1.7");
        let id = doc.add_object(PdfObject::Null);
        let indexed = |base| {
            PdfObject::Array(vec![
                PdfObject::Name("Indexed".into()),
                base,
                PdfObject::Integer(1),
                PdfObject::HexString(vec![0, 255]),
            ])
        };
        doc.set_object(id, indexed(PdfObject::Reference(id)));
        assert!(parse_color_space(&doc, &PdfObject::Reference(id), 0).is_none());

        let mut space = PdfObject::Name("DeviceGray".into());
        for _ in 0..MAX_COLOR_SPACE_DEPTH {
            space = indexed(space);
        }
        assert!(parse_color_space(&doc, &space, 0).is_none());
        assert!(
            parse_color_space(&doc, &indexed(PdfObject::Name("DeviceRGB".into())), 0).is_some()
        );
    }

    #[test]
    fn invalid_dimensions_and_sample_metadata_do_not_overflow() {
        let doc = PdfDocument::new_empty("1.7");
        for (width, height) in [(-1, 1), (1, -1), (0, 1), (i64::MAX, i64::MAX)] {
            let mut stream = gray_stream(1);
            stream
                .dictionary
                .insert("Width".into(), PdfObject::Integer(width));
            stream
                .dictionary
                .insert("Height".into(), PdfObject::Integer(height));
            assert!(decode_image(&doc, &stream).is_none());
        }
        for bpc in [-1, 0, 3, 32, i64::MAX] {
            let mut stream = gray_stream(1);
            stream
                .dictionary
                .insert("BitsPerComponent".into(), PdfObject::Integer(bpc));
            assert!(decode_image(&doc, &stream).is_none());
        }
        assert!(decode_raw(&[0], usize::MAX, 2, 8, &ColorSpace::Gray, None, false, None).is_none());
        assert!(decode_raw(
            &[0],
            1,
            1,
            8,
            &ColorSpace::Tint(usize::MAX),
            None,
            false,
            None
        )
        .is_none());
        let space = ColorSpace::Indexed(Box::new(ColorSpace::Tint(usize::MAX)), Vec::new());
        let rgb = space.to_rgb(&[0.0], u32::MAX);
        assert_eq!((rgb.r, rgb.g, rgb.b), (0.0, 0.0, 0.0));
    }

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
        let image = decode_raw(&data, 3, 1, 8, &space, None, false, None).unwrap();
        // One byte a pixel, and sampled back as gray.
        assert!(image.gray);
        assert_eq!(image.rgb, [0, 128, 255]);
        assert_eq!(image.sample(1, 0).unwrap().0, Rgb::gray(128.0 / 255.0));
    }

    #[test]
    fn indexed_palette_maps_through_the_lookup_table() {
        // Two-entry RGB palette: red then green.
        let space = ColorSpace::Indexed(Box::new(ColorSpace::Rgb), vec![255, 0, 0, 0, 255, 0]);
        // 1 bit per pixel: 0, 1 -> red, green (packed into one byte).
        let image = decode_raw(&[0b0100_0000], 2, 1, 1, &space, None, false, None).unwrap();
        assert_eq!(&image.rgb[0..3], &[255, 0, 0]);
        assert_eq!(&image.rgb[3..6], &[0, 255, 0]);
    }

    #[test]
    fn stencil_mask_paints_where_the_bit_is_zero() {
        let image =
            decode_raw(&[0b0100_0000], 2, 1, 1, &ColorSpace::Gray, None, true, None).unwrap();
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
            None,
        )
        .unwrap();
        assert_eq!(image.alpha, vec![0, 255]);
    }

    #[test]
    fn cmyk_samples_convert_to_rgb() {
        // Pure cyan: C=1 M=0 Y=0 K=0.
        let image = decode_raw(
            &[255, 0, 0, 0],
            1,
            1,
            8,
            &ColorSpace::Cmyk,
            None,
            false,
            None,
        )
        .unwrap();
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
