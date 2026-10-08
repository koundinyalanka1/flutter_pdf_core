//! What recognition found on a page, in image pixel coordinates.

use serde::{Deserialize, Serialize};

/// A recognized word. `quad` holds its corners in reading orientation:
/// top-left, top-right, bottom-right, bottom-left (a rectangle rotated by the
/// page skew); `bounds` is the axis-aligned box around them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub confidence: f32,
    pub quad: [[f64; 2]; 4],
    pub bounds: [f64; 4],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrLine {
    pub text: String,
    pub confidence: f32,
    pub bounds: [f64; 4],
    pub words: Vec<OcrWord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrPage {
    /// Size of the recognized image, in pixels.
    pub width: f64,
    pub height: f64,
    /// Skew corrected before recognition, in degrees (text fell to the right
    /// when positive).
    pub skew_degrees: f64,
    /// Clockwise turn (0, 90, 180 or 270 degrees) that brought the text
    /// upright: 90 means the page was scanned lying on its left side.
    pub orientation_degrees: u16,
    /// Lines in reading order.
    pub lines: Vec<OcrLine>,
}

impl OcrPage {
    /// The page's text, one line per line.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The same page measured in other units: x coordinates times `fx`,
    /// y coordinates times `fy`.
    pub fn scaled(&self, fx: f64, fy: f64) -> OcrPage {
        let scale = |b: [f64; 4]| [b[0] * fx, b[1] * fy, b[2] * fx, b[3] * fy];
        OcrPage {
            width: self.width * fx,
            height: self.height * fy,
            skew_degrees: self.skew_degrees,
            orientation_degrees: self.orientation_degrees,
            lines: self
                .lines
                .iter()
                .map(|line| OcrLine {
                    text: line.text.clone(),
                    confidence: line.confidence,
                    bounds: scale(line.bounds),
                    words: line
                        .words
                        .iter()
                        .map(|w| OcrWord {
                            text: w.text.clone(),
                            confidence: w.confidence,
                            quad: w.quad.map(|[x, y]| [x * fx, y * fy]),
                            bounds: scale(w.bounds),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
