//! The engine held to the training code: `tools/ocr_train/export.py` wrote
//! these fixtures with the Python normalization and PyTorch, using the
//! float16 weights of the embedded model. Re-export them with the model.

use pdf_ocr::ctc::greedy;
use pdf_ocr::image::GrayImage;
use pdf_ocr::net::Network;
use pdf_ocr::normalize::{crop_rect, normalize, Crop, HEIGHT};

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], magic: &[u8]) -> Self {
        assert_eq!(&bytes[..magic.len()], magic);
        Self {
            bytes,
            at: magic.len(),
        }
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let slice = &self.bytes[self.at..self.at + n];
        self.at += n;
        slice
    }
    fn u32(&mut self) -> usize {
        u32::from_le_bytes(self.take(4).try_into().unwrap()) as usize
    }
    fn f64(&mut self) -> f64 {
        f64::from_le_bytes(self.take(8).try_into().unwrap())
    }
    fn f32s(&mut self, n: usize) -> Vec<f32> {
        self.take(4 * n)
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }
}

fn max_difference(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

#[test]
fn normalization_matches_the_training_code() {
    let mut r = Reader::new(include_bytes!("fixtures/golden_normalize.bin"), b"PDFOCRN1");
    let cases = r.u32();
    assert!(cases >= 3);
    for case in 0..cases {
        let (w, h) = (r.u32(), r.u32());
        let image = GrayImage::new(w, h, r.take(w * h).to_vec()).unwrap();
        let ink = [r.f64(), r.f64(), r.f64(), r.f64()];
        let crop = Crop {
            x0: r.u32(),
            y0: r.u32(),
            x1: r.u32(),
            y1: r.u32(),
        };
        let width = r.u32();
        let expected = r.f32s(HEIGHT * width);
        assert_eq!(crop_rect(ink, w, h), crop, "case {case}");
        let line = normalize(&image, crop);
        assert_eq!(line.width, width, "case {case}");
        let difference = max_difference(&line.data, &expected);
        assert!(difference < 1e-5, "case {case}: differs by {difference}");
    }
}

#[test]
fn network_matches_pytorch_for_the_embedded_model() {
    let net = Network::from_bytes(include_bytes!("../models/latin.ocrm")).unwrap();
    let mut r = Reader::new(include_bytes!("fixtures/golden_net.bin"), b"PDFOCRG1");
    let (height, width) = (r.u32(), r.u32());
    assert_eq!(height, net.height);
    let input = r.f32s(height * width);
    let (steps, classes) = (r.u32(), r.u32());
    let expected = r.f32s(steps * classes);
    let text_len = r.u32();
    let expected_text = String::from_utf8(r.take(text_len).to_vec()).unwrap();

    let logits = net.run(&input, width);
    assert_eq!(logits.len(), steps * classes);
    let difference = max_difference(&logits, &expected);
    assert!(difference < 2e-3, "logits differ by {difference}");
    // The golden line's own steps exclude the trailing paper.
    let line_steps = steps - net.trailing_paper / net.stride;
    let text: String = greedy(&logits, line_steps, &net.charset)
        .iter()
        .map(|c| c.ch)
        .collect();
    assert_eq!(text, expected_text);
}
