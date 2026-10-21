//! Text and pictures drawn onto pages: page numbers, headers and footers,
//! watermarks. Each is placed on the page as it is shown, so it stands
//! upright on rotated pages too.

use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_text::text_state::Matrix;

use crate::annots::{draw_on_page, unshown};
use crate::content::{dict, ext_gstate, form, Content};
use crate::editor::Editor;
use crate::font::{self, win_ansi, TextFont};
use crate::frame::Frame;
use crate::model::{Mark, Position};

pub fn add(editor: &mut Editor, pages: &[ObjectId], chosen: &[usize], mark: &Mark, warnings: &mut Vec<String>) -> Result<()> {
    let total = pages.len();
    let font = TextFont::choice(mark.font, mark.bold);
    let size = if mark.font_size.is_finite() { mark.font_size.clamp(1.0, 500.0) } else { 12.0 };
    let margin = if mark.margin.is_finite() { mark.margin.max(0.0) } else { 28.0 };
    let opacity = if mark.opacity.is_finite() { mark.opacity.clamp(0.0, 1.0) } else { 1.0 };
    let image = match &mark.image {
        Some(source) => Some(crate::image::add(editor, source)?),
        None => None,
    };
    if image.is_none() && mark.text.trim().is_empty() {
        return Err(PdfError::Structure("a mark needs text or a picture".into()));
    }
    for &index in chosen {
        let page = pages[index];
        let frame = Frame::of(&editor.doc, page)?;
        let (pw, ph) = (frame.width, frame.height);
        // What is drawn, its size, and how to draw it at the origin.
        let (w, h) = match &image {
            Some((_, iw, ih)) => match mark.size {
                Some([w, h]) if w > 0.0 && h > 0.0 => (w, h),
                _ => {
                    // The picture's proportions, a quarter of the page wide.
                    let w = pw / 4.0;
                    (w, w * *ih as f64 / (*iw).max(1) as f64)
                }
            },
            None => (0.0, 0.0),
        };
        let text = mark.text.replace("{page}", &(index + 1).to_string()).replace("{pages}", &total.to_string());
        let (bytes, lossless) = win_ansi(&text);
        if !lossless {
            warnings.push(unshown());
        }
        let lines = font.wrap(&bytes, size, f64::INFINITY);
        let line_gap = size * 1.2;
        let (bw, bh) = if image.is_some() {
            (w, h)
        } else {
            let widest = lines.iter().map(|l| font.width(l, size)).fold(0.0, f64::max);
            (widest, size + line_gap * (lines.len().max(1) - 1) as f64)
        };
        // The box's centre on the displayed page, y up from the bottom.
        let cx = match mark.position {
            Position::TopLeft | Position::BottomLeft => margin + bw / 2.0,
            Position::TopRight | Position::BottomRight => pw - margin - bw / 2.0,
            _ => pw / 2.0,
        };
        let cy = match mark.position {
            Position::TopLeft | Position::TopCenter | Position::TopRight => ph - margin - bh / 2.0,
            Position::BottomLeft | Position::BottomCenter | Position::BottomRight => margin + bh / 2.0,
            Position::Center => ph / 2.0,
        };
        let mut content = Content::new();
        let mut resources = Dictionary::new();
        if opacity < 1.0 {
            resources.insert("ExtGState".into(), PdfObject::Dictionary(dict(vec![("GS0", ext_gstate(opacity, opacity, None))])));
            content.gs("GS0");
        }
        // The whole displayed page, y up, then the box's centre, turned.
        content.cm(&frame.upright([0.0, 0.0, pw, ph]));
        let turn = mark.rotation.to_radians();
        let (sin, cos) = if turn.is_finite() { turn.sin_cos() } else { (0.0, 1.0) };
        content.cm(&Matrix::new(cos, sin, -sin, cos, cx, cy));
        match &image {
            Some((reference, _, _)) => {
                resources.insert("XObject".into(), PdfObject::Dictionary(dict(vec![("Im0", reference.clone())])));
                content.cm(&Matrix::new(w, 0.0, 0.0, h, -w / 2.0, -h / 2.0)).raw("/Im0 Do");
            }
            None => {
                resources.insert("Font".into(), PdfObject::Dictionary(dict(vec![("F0", font.dict())])));
                content.fill_color(mark.color);
                // Baselines from the top line down, cap height centred.
                let mut y = bh / 2.0 - size * 0.72;
                for line in &lines {
                    let lw = font.width(line, size);
                    let x = match mark.position {
                        Position::TopLeft | Position::BottomLeft => -bw / 2.0,
                        Position::TopRight | Position::BottomRight => bw / 2.0 - lw,
                        _ => -lw / 2.0,
                    };
                    font::show(&mut content, "F0", size, x, y, line);
                    y -= line_gap;
                }
            }
        }
        let user_box = frame.user_rect([0.0, 0.0, pw, ph]);
        let drawing = editor.add(form(user_box, None, resources, &content));
        draw_on_page(editor, page, drawing)?;
    }
    Ok(())
}
