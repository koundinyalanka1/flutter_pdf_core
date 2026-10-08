//! Optical character recognition for printed text, from scratch.
//!
//! Classical image analysis finds the text (adaptive binarization, connected
//! components, skew measurement, line grouping, reading order) and a small
//! neural network trained for this library reads each line (a CNN and a
//! bidirectional LSTM decoded with CTC). Inference is plain Rust with no
//! third-party OCR or machine-learning code; the model was trained by
//! `tools/ocr_train` on synthetic lines rendered from open-licensed fonts.
//!
//! The bundled model reads Latin-script text: English and the Western
//! European languages, digits, and common document punctuation and symbols.

pub mod binarize;
pub mod components;
pub mod ctc;
pub mod engine;
mod glyphless;
pub mod image;
pub mod layer;
pub mod layout;
pub mod net;
pub mod normalize;
pub mod pdf;
pub mod result;

pub use engine::{OcrEngine, OcrOptions};
pub use image::GrayImage;
pub use net::ModelError;
pub use pdf::{PageOcr, PageReport, PageStatus, PdfOcrOptions};
pub use result::{OcrLine, OcrPage, OcrWord};
