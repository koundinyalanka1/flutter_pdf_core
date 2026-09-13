//! Milestone 13 (part 2): page rasterization.
//!
//! A from-scratch PDF rasterizer: scanline anti-aliased path filling, clipping
//! masks, image XObjects and TrueType text, driven by the same content-stream
//! parser the text extractor uses.

pub mod canvas;
pub mod font;
pub mod geom;
pub mod image;
pub mod page;
pub mod png;

pub use canvas::Rgb;
pub use png::encode_rgba_as_png;
pub use page::{page_size_points, render_page, RenderOptions, RenderSize, RenderedPage};
