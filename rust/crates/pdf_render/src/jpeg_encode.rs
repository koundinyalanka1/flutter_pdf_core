//! A baseline JPEG encoder, for compressing a PDF's pictures and saving
//! pages as photos.
//!
//! Gray or YCbCr with the colour at half resolution both ways (4:2:0),
//! the quantization tables of the JPEG standard's Annex K scaled by a
//! quality from 1 to 100 as the IJG library does, and Huffman tables made
//! for each image from its own symbols (Annex K.2), which are smaller than
//! the standard's general-purpose tables.

/// Annex K.1's luminance and chrominance quantization tables, in natural
/// (row-major) order.
const LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51,
    87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99,
];

/// The natural index of each coefficient in zigzag order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47,
    55, 62, 63,
];

fn scaled(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = u32::from(quality.clamp(1, 100));
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for (o, b) in out.iter_mut().zip(base) {
        *o = ((u32::from(*b) * scale + 50) / 100).clamp(1, 255) as u16;
    }
    out
}

/// The 8×8 forward DCT as two passes of a cosine table.
struct Dct {
    table: [[f32; 8]; 8],
}

impl Dct {
    fn new() -> Self {
        let mut table = [[0f32; 8]; 8];
        for (u, row) in table.iter_mut().enumerate() {
            let c = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
            for (x, v) in row.iter_mut().enumerate() {
                *v = c / 2.0 * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
            }
        }
        Dct { table }
    }

    /// Level-shifted samples to quantized coefficients in zigzag order.
    fn block(&self, samples: &[f32; 64], quant: &[u16; 64]) -> [i16; 64] {
        let mut rows = [0f32; 64];
        for y in 0..8 {
            for u in 0..8 {
                let mut s = 0.0;
                for x in 0..8 {
                    s += self.table[u][x] * samples[y * 8 + x];
                }
                rows[y * 8 + u] = s;
            }
        }
        let mut coeffs = [0f32; 64];
        for u in 0..8 {
            for v in 0..8 {
                let mut s = 0.0;
                for y in 0..8 {
                    s += self.table[v][y] * rows[y * 8 + u];
                }
                coeffs[v * 8 + u] = s;
            }
        }
        let mut out = [0i16; 64];
        for (k, &natural) in ZIGZAG.iter().enumerate() {
            out[k] = (coeffs[natural] / f32::from(quant[natural])).round() as i16;
        }
        out
    }
}

/// The bits a value takes, and those bits as JPEG writes them.
fn magnitude(v: i32) -> (u8, u16) {
    let size = (32 - v.unsigned_abs().leading_zeros()) as u8;
    let bits = if v < 0 { (v - 1) as u16 & ((1u32 << size) - 1) as u16 } else { v as u16 };
    (size, bits)
}

/// One Huffman table: counts by code length, symbols, and each symbol's
/// code.
struct Huffman {
    bits: [u8; 16],
    values: Vec<u8>,
    codes: [(u16, u8); 256],
}

impl Huffman {
    /// The optimal table for these symbol counts, codes at most 16 bits
    /// long (ISO 10918-1 K.2).
    fn optimal(counts: &[u32; 256]) -> Huffman {
        let mut freq = [0i64; 257];
        for (f, c) in freq.iter_mut().zip(counts) {
            *f = i64::from(*c);
        }
        // A reserved symbol, so no real code is all ones.
        freq[256] = 1;
        let mut size = [0usize; 257];
        let mut others = [-1i32; 257];
        loop {
            let mut c1: i32 = -1;
            let mut v = i64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f > 0 && f <= v {
                    v = f;
                    c1 = i as i32;
                }
            }
            let mut c2: i32 = -1;
            v = i64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f > 0 && f <= v && i as i32 != c1 {
                    v = f;
                    c2 = i as i32;
                }
            }
            if c2 < 0 {
                break;
            }
            let (a, b) = (c1 as usize, c2 as usize);
            freq[a] += freq[b];
            freq[b] = 0;
            let mut k = a;
            size[k] += 1;
            while others[k] >= 0 {
                k = others[k] as usize;
                size[k] += 1;
            }
            others[k] = c2;
            let mut k = b;
            size[k] += 1;
            while others[k] >= 0 {
                k = others[k] as usize;
                size[k] += 1;
            }
        }
        let mut bits = [0usize; 33];
        for &s in &size {
            if s > 0 {
                bits[s.min(32)] += 1;
            }
        }
        // Lengths over 16 are folded in, keeping the code complete.
        for i in (17..=32).rev() {
            while bits[i] > 0 {
                let mut j = i - 2;
                while bits[j] == 0 {
                    j -= 1;
                }
                bits[i] -= 2;
                bits[i - 1] += 1;
                bits[j + 1] += 2;
                bits[j] -= 1;
            }
        }
        // Take the reserved symbol back out of the longest length.
        let mut i = 16;
        while i > 0 && bits[i] == 0 {
            i -= 1;
        }
        if i > 0 {
            bits[i] -= 1;
        }
        let mut values = Vec::new();
        for length in 1..=32 {
            for (symbol, &s) in size.iter().enumerate().take(256) {
                if s == length {
                    values.push(symbol as u8);
                }
            }
        }
        let mut table = Huffman { bits: [0; 16], values, codes: [(0, 0); 256] };
        for l in 0..16 {
            table.bits[l] = bits[l + 1] as u8;
        }
        // Canonical codes, shortest first (Annex C).
        let mut code = 0u16;
        let mut k = 0;
        for l in 0..16 {
            for _ in 0..table.bits[l] {
                if let Some(&symbol) = table.values.get(k) {
                    table.codes[symbol as usize] = (code, l as u8 + 1);
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        table
    }
}

/// Entropy-coded bits, with 0xFF bytes stuffed.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    count: u32,
}

impl BitWriter {
    fn put(&mut self, bits: u16, length: u8) {
        if length == 0 {
            return;
        }
        self.acc = (self.acc << length) | (u32::from(bits) & ((1u32 << length) - 1));
        self.count += u32::from(length);
        while self.count >= 8 {
            let byte = (self.acc >> (self.count - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
            self.count -= 8;
        }
        self.acc &= (1u32 << self.count) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            let pad = 8 - self.count as u8;
            self.put((1u16 << pad) - 1, pad);
        }
        self.out
    }
}

/// A component's quantized blocks, in the order they are written.
struct Plane {
    blocks: Vec<[i16; 64]>,
    chroma: bool,
}

/// Encode 8-bit samples, gray (`channels` 1) or RGB (3), row-major from
/// the top left, as a baseline JPEG of `quality` 1-100.
pub fn encode_jpeg(samples: &[u8], width: u32, height: u32, channels: u8, quality: u8) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w > 65_535 || h > 65_535 || !matches!(channels, 1 | 3) {
        return None;
    }
    if samples.len() < w * h * channels as usize {
        return None;
    }
    let luma_q = scaled(&LUMA_Q, quality);
    let chroma_q = scaled(&CHROMA_Q, quality);
    let dct = Dct::new();
    let gray = channels == 1;
    let mcu = if gray { 8 } else { 16 };
    let (mcus_x, mcus_y) = (w.div_ceil(mcu), h.div_ceil(mcu));
    // Components as planes of f32, edges repeated.
    let at = |x: usize, y: usize| (y.min(h - 1) * w + x.min(w - 1)) * channels as usize;
    let mut planes: Vec<Plane> = Vec::new();
    // Blocks interleaved MCU by MCU: four Y, one Cb, one Cr (or one Y).
    let mut order: Vec<(usize, [i16; 64])> = Vec::with_capacity(mcus_x * mcus_y * if gray { 1 } else { 6 });
    planes.push(Plane { blocks: Vec::new(), chroma: false });
    if !gray {
        planes.push(Plane { blocks: Vec::new(), chroma: true });
        planes.push(Plane { blocks: Vec::new(), chroma: true });
    }
    let ycc = |x: usize, y: usize| -> (f32, f32, f32) {
        let i = at(x, y);
        let (r, g, b) = (f32::from(samples[i]), f32::from(samples[i + 1]), f32::from(samples[i + 2]));
        (
            0.299 * r + 0.587 * g + 0.114 * b,
            -0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0,
            0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0,
        )
    };
    let mut block = [0f32; 64];
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if gray {
                for y in 0..8 {
                    for x in 0..8 {
                        block[y * 8 + x] = f32::from(samples[at(mx * 8 + x, my * 8 + y)]) - 128.0;
                    }
                }
                order.push((0, dct.block(&block, &luma_q)));
                continue;
            }
            for (by, bx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                for y in 0..8 {
                    for x in 0..8 {
                        block[y * 8 + x] = ycc(mx * 16 + bx * 8 + x, my * 16 + by * 8 + y).0 - 128.0;
                    }
                }
                order.push((0, dct.block(&block, &luma_q)));
            }
            let mut cb = [0f32; 64];
            let mut cr = [0f32; 64];
            for y in 0..8 {
                for x in 0..8 {
                    let (mut sb, mut sr) = (0.0, 0.0);
                    for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                        let (_, b, r) = ycc(mx * 16 + x * 2 + dx, my * 16 + y * 2 + dy);
                        sb += b;
                        sr += r;
                    }
                    cb[y * 8 + x] = sb / 4.0 - 128.0;
                    cr[y * 8 + x] = sr / 4.0 - 128.0;
                }
            }
            order.push((1, dct.block(&cb, &chroma_q)));
            order.push((2, dct.block(&cr, &chroma_q)));
        }
    }
    for (component, b) in &order {
        planes[*component].blocks.push(*b);
    }

    // Count symbols for the tables (luma and chroma separately).
    let mut dc_counts = [[0u32; 256]; 2];
    let mut ac_counts = [[0u32; 256]; 2];
    let mut previous = [0i32; 3];
    for (component, b) in &order {
        let t = usize::from(planes[*component].chroma);
        let diff = i32::from(b[0]) - previous[*component];
        previous[*component] = i32::from(b[0]);
        dc_counts[t][magnitude(diff).0 as usize] += 1;
        let mut run = 0;
        for &c in &b[1..] {
            if c == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                ac_counts[t][0xF0] += 1;
                run -= 16;
            }
            ac_counts[t][(run << 4) | magnitude(i32::from(c)).0 as usize] += 1;
            run = 0;
        }
        if run > 0 {
            ac_counts[t][0] += 1;
        }
    }
    let tables = if gray { 1 } else { 2 };
    let dc: Vec<Huffman> = (0..tables).map(|t| Huffman::optimal(&dc_counts[t])).collect();
    let ac: Vec<Huffman> = (0..tables).map(|t| Huffman::optimal(&ac_counts[t])).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&[0xFF, 0xD8]);
    // JFIF, so the file stands on its own as a photo.
    out.extend_from_slice(&[0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0]);
    for (id, q) in [(0u8, &luma_q), (1u8, &chroma_q)].into_iter().take(tables) {
        out.extend_from_slice(&[0xFF, 0xDB, 0, 67, id]);
        for &natural in &ZIGZAG {
            out.push(q[natural] as u8);
        }
    }
    let components = if gray { 1u8 } else { 3 };
    out.extend_from_slice(&[0xFF, 0xC0]);
    out.extend_from_slice(&(8 + 3 * u16::from(components)).to_be_bytes());
    out.push(8);
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.push(components);
    if gray {
        out.extend_from_slice(&[1, 0x11, 0]);
    } else {
        out.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    }
    for (class, list) in [(0u8, &dc), (1u8, &ac)] {
        for (id, table) in list.iter().enumerate() {
            out.extend_from_slice(&[0xFF, 0xC4]);
            out.extend_from_slice(&(3 + 16 + table.values.len() as u16).to_be_bytes());
            out.push((class << 4) | id as u8);
            out.extend_from_slice(&table.bits);
            out.extend_from_slice(&table.values);
        }
    }
    out.extend_from_slice(&[0xFF, 0xDA]);
    out.extend_from_slice(&(6 + 2 * u16::from(components)).to_be_bytes());
    out.push(components);
    if gray {
        out.extend_from_slice(&[1, 0x00]);
    } else {
        out.extend_from_slice(&[1, 0x00, 2, 0x11, 3, 0x11]);
    }
    out.extend_from_slice(&[0, 63, 0]);

    let mut writer = BitWriter { out: Vec::with_capacity(w * h / 4), acc: 0, count: 0 };
    let mut previous = [0i32; 3];
    for (component, b) in &order {
        let t = usize::from(planes[*component].chroma);
        let (dct_table, act) = (&dc[t], &ac[t]);
        let diff = i32::from(b[0]) - previous[*component];
        previous[*component] = i32::from(b[0]);
        let (size, bits) = magnitude(diff);
        let (code, length) = dct_table.codes[size as usize];
        writer.put(code, length);
        writer.put(bits, size);
        let mut run = 0;
        for &c in &b[1..] {
            if c == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                let (code, length) = act.codes[0xF0];
                writer.put(code, length);
                run -= 16;
            }
            let (size, bits) = magnitude(i32::from(c));
            let (code, length) = act.codes[(run << 4) | size as usize];
            writer.put(code, length);
            writer.put(bits, size);
            run = 0;
        }
        if run > 0 {
            let (code, length) = act.codes[0];
            writer.put(code, length);
        }
    }
    out.extend_from_slice(&writer.finish());
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some(out)
}

/// A rendered page (RGBA, opaque) as a JPEG.
pub fn encode_rgba_as_jpeg(rgba: &[u8], width: u32, height: u32, quality: u8) -> Option<Vec<u8>> {
    let pixels = width as usize * height as usize;
    if rgba.len() < pixels * 4 {
        return None;
    }
    let rgb: Vec<u8> = rgba.chunks_exact(4).take(pixels).flat_map(|p| [p[0], p[1], p[2]]).collect();
    encode_jpeg(&rgb, width, height, 3, quality)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Width, height and RGB samples, through the decoder the renderer uses.
    fn decode(jpeg: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let pixels = decoder.decode().ok()?;
        let info = decoder.info()?;
        let rgb = match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => pixels.iter().flat_map(|g| [*g, *g, *g]).collect(),
            jpeg_decoder::PixelFormat::RGB24 => pixels,
            _ => return None,
        };
        Some((usize::from(info.width), usize::from(info.height), rgb))
    }

    /// Peak signal-to-noise between two sample buffers, in decibels.
    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
    }

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let ring = ((((x as f64 - 40.0).powi(2) + (y as f64 - 30.0).powi(2)).sqrt() / 6.0).sin() * 60.0) as i32;
                rgb.push((x * 255 / w) as u8);
                rgb.push((y * 255 / h) as u8);
                rgb.push((128 + ring).clamp(0, 255) as u8);
            }
        }
        rgb
    }

    #[test]
    fn colour_round_trips_through_the_decoder() {
        let (w, h) = (83, 61);
        let rgb = gradient(w, h);
        let jpeg = encode_jpeg(&rgb, w as u32, h as u32, 3, 90).unwrap();
        let decoded = decode(&jpeg).expect("decodes");
        assert_eq!((decoded.0, decoded.1), (w, h));
        assert!(psnr(&rgb, &decoded.2) > 30.0, "psnr {}", psnr(&rgb, &decoded.2));
        // Lower quality is smaller.
        let small = encode_jpeg(&rgb, w as u32, h as u32, 3, 30).unwrap();
        assert!(small.len() < jpeg.len());
    }

    #[test]
    fn gray_round_trips() {
        let (w, h) = (40, 33);
        let gray: Vec<u8> = (0..w * h).map(|i| ((i % w) * 6) as u8).collect();
        let jpeg = encode_jpeg(&gray, w as u32, h as u32, 1, 95).unwrap();
        let decoded = decode(&jpeg).expect("decodes");
        let back: Vec<u8> = decoded.2.chunks_exact(3).map(|p| p[0]).collect();
        assert!(psnr(&gray, &back) > 35.0, "psnr {}", psnr(&gray, &back));
    }

    #[test]
    fn huffman_codes_fit_sixteen_bits_and_are_prefix_free() {
        // A skewed distribution that would want long codes.
        let mut counts = [0u32; 256];
        for (i, c) in counts.iter_mut().enumerate() {
            *c = 1 << (i % 24);
        }
        let table = Huffman::optimal(&counts);
        assert_eq!(table.values.len(), 256);
        let codes: Vec<(u16, u8)> = (0..256).map(|s| table.codes[s]).collect();
        assert!(codes.iter().all(|(_, l)| (1..=16).contains(l)));
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                let (short, long) = if a.1 <= b.1 { (a, b) } else { (b, a) };
                assert_ne!(long.0 >> (long.1 - short.1), short.0, "a prefix");
            }
        }
    }
}
