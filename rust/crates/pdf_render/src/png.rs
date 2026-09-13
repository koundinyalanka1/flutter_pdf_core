//! A minimal PNG encoder for rendered pages.
//!
//! Rendered pages cross the FFI boundary twice over: the viewer wants raw
//! pixels (straight into `ui.decodeImageFromPixels`), while thumbnails want
//! something compact that Dart can hold as an ordinary `Uint8List`. PNG is
//! that second form, and it is cheap to produce here — a rendered page is
//! already opaque, so this writes 8-bit RGB with the standard per-row filter
//! heuristic and zlib from the same `flate2` the PDF filters use.

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::{Compression, Crc};

/// Encode an RGBA8 buffer (as produced by the rasterizer) as an RGB PNG.
pub fn encode_rgba_as_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return None;
    }

    // Filter each row, choosing the filter with the smallest absolute sum —
    // the heuristic the PNG spec itself recommends.
    let stride = w * 3;
    let mut raw = Vec::with_capacity((stride + 1) * h);
    let mut row = vec![0u8; stride];
    let mut previous = vec![0u8; stride];
    let mut candidates = [vec![0u8; stride], vec![0u8; stride], vec![0u8; stride]];

    for y in 0..h {
        for x in 0..w {
            let src = (y * w + x) * 4;
            row[x * 3] = rgba[src];
            row[x * 3 + 1] = rgba[src + 1];
            row[x * 3 + 2] = rgba[src + 2];
        }

        // 0 = None, 1 = Sub (left), 2 = Up (previous row).
        let mut best = (0usize, u64::MAX);
        for (index, filter) in [0u8, 1, 2].iter().enumerate() {
            let buffer = &mut candidates[index];
            let mut score = 0u64;
            for i in 0..stride {
                let value = match filter {
                    1 => row[i].wrapping_sub(if i >= 3 { row[i - 3] } else { 0 }),
                    2 => row[i].wrapping_sub(previous[i]),
                    _ => row[i],
                };
                buffer[i] = value;
                score += (value as i8).unsigned_abs() as u64;
            }
            if score < best.1 {
                best = (index, score);
            }
        }

        raw.push(best.0 as u8);
        raw.extend_from_slice(&candidates[best.0]);
        previous.copy_from_slice(&row);
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&raw).ok()?;
    let compressed = encoder.finish().ok()?;

    let mut png = Vec::with_capacity(compressed.len() + 64);
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // colour type 2 = truecolour RGB
    ihdr.extend_from_slice(&[0, 0, 0]); // deflate, adaptive filtering, no interlace
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &compressed);
    write_chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);

    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_well_formed_png_header() {
        let rgba = vec![255u8; 4 * 4 * 4];
        let png = encode_rgba_as_png(&rgba, 4, 4).unwrap();
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes([png[16], png[17], png[18], png[19]]), 4);
        assert_eq!(u32::from_be_bytes([png[20], png[21], png[22], png[23]]), 4);
        assert_eq!(png[24], 8, "bit depth");
        assert_eq!(png[25], 2, "colour type RGB");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn rejects_short_buffers_and_zero_extents() {
        assert!(encode_rgba_as_png(&[0, 0, 0], 4, 4).is_none());
        assert!(encode_rgba_as_png(&[], 0, 0).is_none());
    }

    #[test]
    fn flat_colour_compresses_far_below_the_raw_size() {
        let rgba = vec![0u8; 200 * 200 * 4];
        let png = encode_rgba_as_png(&rgba, 200, 200).unwrap();
        assert!(
            png.len() < 200 * 200 * 3 / 20,
            "expected heavy compression, got {} bytes",
            png.len()
        );
    }

    #[test]
    fn round_trips_through_a_decoder() {
        // Build a recognisable 2x1 image: red then green.
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let png = encode_rgba_as_png(&rgba, 2, 1).unwrap();
        // Re-inflate the IDAT and check the filtered scanline.
        let idat_start = 8 + 25 + 8; // signature + IHDR chunk + IDAT header
        let idat_len = u32::from_be_bytes([png[33], png[34], png[35], png[36]]) as usize;
        let compressed = &png[idat_start..idat_start + idat_len];
        let inflated = pdf_core::filter::flate_decode(compressed).unwrap();
        assert_eq!(inflated[0], 0, "filter byte");
        assert_eq!(&inflated[1..7], &[255, 0, 0, 0, 255, 0]);
    }
}
