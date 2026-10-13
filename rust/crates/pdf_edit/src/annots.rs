//! Annotations: listing them, adding them with their appearances, removing
//! them, and drawing an appearance into a page for good.

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_text::text_state::Matrix;

use crate::content::{
    color_array, decode_text, dict, ext_gstate, form, name, now, numbers, real, text_string, unique_name, Content, OURS,
};
use crate::editor::Editor;
use crate::font::{self, win_ansi, TextFont};
use crate::frame::{bounds, normal, Frame};
use crate::model::{AnnotInfo, Annotation, Color, Kind};

/// The annotations on a page (0-based), as displayed.
pub fn annotations(doc: &PdfDocument, page_index: usize) -> Result<Vec<AnnotInfo>> {
    let pages = doc.collect_page_ids().unwrap_or_default();
    let page = *pages.get(page_index).ok_or(PdfError::PageIndex(page_index))?;
    let frame = Frame::of(doc, page)?;
    let Some(page_dict) = doc.resolve(page).and_then(PdfObject::as_dict) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let list = page_dict.get("Annots").map(|a| doc.resolve_value(a));
    let Some(PdfObject::Array(items)) = list else { return Ok(out) };
    for item in items {
        let Some(id) = item.as_ref() else { continue };
        let Some(annot) = doc.resolve(id).and_then(PdfObject::as_dict) else { continue };
        let subtype = annot.get("Subtype").and_then(PdfObject::as_name).unwrap_or("").to_owned();
        if subtype == "Popup" {
            continue;
        }
        let Some(rect) = rect_of(doc, annot.get("Rect")) else { continue };
        let text = |key: &str| match annot.get(key).map(|v| doc.resolve_value(v)) {
            Some(PdfObject::LiteralString(b) | PdfObject::HexString(b)) => decode_text(&b),
            _ => String::new(),
        };
        let color = match annot.get("C").map(|v| doc.resolve_value(v)) {
            Some(PdfObject::Array(parts)) if parts.len() == 3 => {
                let v: Vec<f64> = parts.iter().filter_map(number).collect();
                (v.len() == 3).then(|| Color(v[0], v[1], v[2]).hex())
            }
            _ => None,
        };
        out.push(AnnotInfo {
            id: format!("{} {}", id.number, id.generation),
            subtype,
            rect: frame.display_rect(rect),
            contents: text("Contents"),
            author: text("T"),
            color,
            ours: text("NM").starts_with(OURS),
        });
    }
    Ok(out)
}

fn number(v: &PdfObject) -> Option<f64> {
    match v {
        PdfObject::Integer(i) => Some(*i as f64),
        PdfObject::Real(r) => Some(*r),
        _ => None,
    }
}

pub(crate) fn rect_of(doc: &PdfDocument, value: Option<&PdfObject>) -> Option<[f64; 4]> {
    let PdfObject::Array(parts) = doc.resolve_value(value?) else { return None };
    let v: Vec<f64> = parts.iter().filter_map(|p| number(&doc.resolve_value(p))).collect();
    (v.len() >= 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

/// "12 0" as an object id.
fn parse_id(text: &str) -> Option<ObjectId> {
    let mut parts = text.split_whitespace();
    let number = parts.next()?.parse().ok()?;
    let generation = parts.next().map_or(Some(0), |g| g.parse().ok())?;
    Some(ObjectId::new(number, generation))
}

/// The page's /Annots array, and where it is kept when it is an object of
/// its own.
fn annots_of(editor: &Editor, page: &Dictionary) -> (Vec<PdfObject>, Option<ObjectId>) {
    match page.get("Annots") {
        Some(PdfObject::Reference(id)) => match editor.doc.resolve(*id) {
            Some(PdfObject::Array(items)) => (items.clone(), Some(*id)),
            _ => (Vec::new(), None),
        },
        Some(PdfObject::Array(items)) => (items.clone(), None),
        _ => (Vec::new(), None),
    }
}

fn set_annots(editor: &mut Editor, page_id: ObjectId, mut page: Dictionary, items: Vec<PdfObject>, own: Option<ObjectId>) {
    match own {
        Some(id) => editor.set(id, PdfObject::Array(items)),
        None => {
            page.insert("Annots".into(), PdfObject::Array(items));
            editor.set(page_id, PdfObject::Dictionary(page));
        }
    }
}

/// Remove an annotation (and its pop-up) from a page.
pub fn remove(editor: &mut Editor, page_id: ObjectId, id: &str) -> Result<()> {
    let target = parse_id(id).ok_or_else(|| PdfError::Structure(format!("bad annotation id {id:?}")))?;
    let page = editor.dict(page_id).ok_or_else(|| PdfError::Structure("page is not a dictionary".into()))?;
    let (items, own) = annots_of(editor, &page);
    let popup = editor.dict(target).and_then(|a| a.get("Popup").and_then(PdfObject::as_ref));
    let kept: Vec<PdfObject> = items
        .iter()
        .filter(|item| {
            let r = item.as_ref();
            r != Some(target) && (popup.is_none() || r != popup)
        })
        .cloned()
        .collect();
    if kept.len() != items.len() {
        set_annots(editor, page_id, page, kept, own);
    }
    Ok(())
}

/// Put an annotation on a page's list.
fn append(editor: &mut Editor, page_id: ObjectId, annot: ObjectId) -> Result<()> {
    let page = editor.dict(page_id).ok_or_else(|| PdfError::Structure("page is not a dictionary".into()))?;
    let (mut items, own) = annots_of(editor, &page);
    items.push(PdfObject::Reference(annot));
    set_annots(editor, page_id, page, items, own);
    Ok(())
}

/// The page's resources: its own or inherited, resolved.
fn resources_of(doc: &PdfDocument, page_id: ObjectId) -> Dictionary {
    let mut node = Some(page_id);
    for _ in 0..64 {
        let Some(id) = node else { break };
        let Some(d) = doc.resolve(id).and_then(PdfObject::as_dict) else { break };
        if let Some(r) = d.get("Resources").and_then(|r| doc.resolve_dict(r)) {
            return r.clone();
        }
        node = d.get("Parent").and_then(PdfObject::as_ref);
    }
    Dictionary::new()
}

/// Draw a form XObject (in the page's user space) on top of a page's
/// content, for good. The page's own content is wrapped in q/Q so what it
/// leaves set cannot move the drawing.
pub(crate) fn draw_on_page(editor: &mut Editor, page_id: ObjectId, form_ref: ObjectId) -> Result<()> {
    let mut page = editor.dict(page_id).ok_or_else(|| PdfError::Structure("page is not a dictionary".into()))?;
    let mut resources = resources_of(&editor.doc, page_id);
    let mut xobjects = resources
        .get("XObject")
        .and_then(|x| editor.doc.resolve_dict(x))
        .cloned()
        .unwrap_or_default();
    let mut n = 0;
    let key = loop {
        let key = format!("DHFm{n}");
        if !xobjects.contains_key(&key) {
            break key;
        }
        n += 1;
    };
    xobjects.insert(key.clone(), PdfObject::Reference(form_ref));
    resources.insert("XObject".into(), PdfObject::Dictionary(xobjects));
    page.insert("Resources".into(), PdfObject::Dictionary(resources));

    let existing: Vec<PdfObject> = match page.get("Contents") {
        Some(PdfObject::Reference(id)) => match editor.doc.resolve(*id) {
            Some(PdfObject::Array(items)) => items.clone(),
            Some(_) => vec![PdfObject::Reference(*id)],
            None => Vec::new(),
        },
        Some(PdfObject::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let open = editor.add(crate::content::flate_stream(Dictionary::new(), b"q\n"));
    let close = editor.add(crate::content::flate_stream(Dictionary::new(), format!("\nQ\nq /{key} Do Q\n").as_bytes()));
    let mut contents = vec![PdfObject::Reference(open)];
    contents.extend(existing);
    contents.push(PdfObject::Reference(close));
    page.insert("Contents".into(), PdfObject::Array(contents));
    editor.set(page_id, PdfObject::Dictionary(page));
    Ok(())
}

/// An appearance being built: what it draws, in user space, and what it
/// needs.
struct Appearance {
    subtype: &'static str,
    /// The annotation's rectangle in user space.
    rect: [f64; 4],
    content: Content,
    resources: Dictionary,
    /// Entries particular to the subtype.
    extra: Dictionary,
}

impl Appearance {
    fn new(subtype: &'static str, rect: [f64; 4]) -> Self {
        Appearance { subtype, rect, content: Content::new(), resources: Dictionary::new(), extra: Dictionary::new() }
    }

    /// Set opacity (and a blend mode) for what follows.
    fn alpha(&mut self, opacity: f64, blend: Option<&str>) {
        if opacity < 1.0 || blend.is_some() {
            let mut states = Dictionary::new();
            states.insert("GS0".into(), ext_gstate(opacity, opacity, blend));
            self.resources.insert("ExtGState".into(), PdfObject::Dictionary(states));
            self.content.gs("GS0");
        }
    }

    fn font(&mut self, font: &TextFont) {
        let mut fonts = Dictionary::new();
        fonts.insert("Helv".into(), font.dict());
        self.resources.insert("Font".into(), PdfObject::Dictionary(fonts));
    }
}

fn grow(r: [f64; 4], by: f64) -> [f64; 4] {
    [r[0] - by, r[1] - by, r[2] + by, r[3] + by]
}

fn points(frame: &Frame, pts: &[[f64; 2]]) -> Vec<(f64, f64)> {
    pts.iter().filter(|p| p[0].is_finite() && p[1].is_finite()).map(|p| frame.user(p[0], p[1])).collect()
}

/// Add an annotation, or draw it into the page when it is to be flattened.
pub fn add(editor: &mut Editor, page_id: ObjectId, a: &Annotation, warnings: &mut Vec<String>) -> Result<()> {
    let frame = Frame::of(&editor.doc, page_id)?;
    let opacity = if a.opacity.is_finite() { a.opacity.clamp(0.0, 1.0) } else { 1.0 };
    let width = if a.width.is_finite() { a.width.clamp(0.0, 100.0) } else { 1.0 };
    let mut ap = match &a.kind {
        Kind::Highlight { rects } | Kind::Underline { rects } | Kind::StrikeOut { rects } | Kind::Squiggly { rects } => {
            markup(&frame, a, rects, opacity)?
        }
        Kind::Ink { strokes } | Kind::Signature { strokes, .. } => {
            let strokes: Vec<Vec<(f64, f64)>> = strokes.iter().map(|s| points(&frame, s)).filter(|s| !s.is_empty()).collect();
            let all: Vec<(f64, f64)> = strokes.iter().flatten().copied().collect();
            if all.is_empty() {
                return Err(PdfError::Structure("a drawing needs at least one point".into()));
            }
            let mut rect = grow(bounds(&all), width.max(1.0));
            let ink = matches!(a.kind, Kind::Ink { .. });
            if let Kind::Signature { rect: r, .. } = &a.kind {
                let given = frame.user_rect(normal(*r));
                rect = [rect[0].min(given[0]), rect[1].min(given[1]), rect[2].max(given[2]), rect[3].max(given[3])];
            }
            let mut ap = Appearance::new(if ink { "Ink" } else { "Stamp" }, rect);
            ap.alpha(opacity, None);
            ap.content.stroke_color(a.color).strokes(&strokes, width.max(0.1));
            if ink {
                let list = strokes
                    .iter()
                    .map(|s| PdfObject::Array(s.iter().flat_map(|p| [real(p.0), real(p.1)]).collect()))
                    .collect();
                ap.extra.insert("InkList".into(), PdfObject::Array(list));
            } else {
                ap.extra.insert("Name".into(), name("DHSignature"));
            }
            ap.extra.insert("BS".into(), PdfObject::Dictionary(dict(vec![("W", real(width))])));
            ap
        }
        Kind::Rectangle { rect } | Kind::Ellipse { rect } => {
            let r = frame.user_rect(normal(*rect));
            let square = matches!(a.kind, Kind::Rectangle { .. });
            let mut ap = Appearance::new(if square { "Square" } else { "Circle" }, r);
            ap.alpha(opacity, None);
            let half = width / 2.0;
            let inner = [r[0] + half, r[1] + half, r[2] - half, r[3] - half];
            ap.content.op(&[width], "w").stroke_color(a.color);
            if let Some(fill) = a.fill {
                ap.content.fill_color(fill);
                ap.extra.insert("IC".into(), color_array(fill));
            }
            if square {
                ap.content.rect(inner[0], inner[1], inner[2] - inner[0], inner[3] - inner[1]);
            } else {
                ap.content.ellipse(inner[0], inner[1], inner[2], inner[3]);
            }
            ap.content.raw(match (a.fill.is_some(), width > 0.0) {
                (true, true) => "B",
                (true, false) => "f",
                _ => "S",
            });
            ap.extra.insert("BS".into(), PdfObject::Dictionary(dict(vec![("W", real(width))])));
            ap
        }
        Kind::Line { from, to } | Kind::Arrow { from, to } => {
            let (p, q) = (frame.user(from[0], from[1]), frame.user(to[0], to[1]));
            let arrow = matches!(a.kind, Kind::Arrow { .. });
            let head = (width * 3.0).max(8.0);
            let mut ap = Appearance::new("Line", grow(bounds(&[p, q]), head + width));
            ap.alpha(opacity, None);
            ap.content.op(&[width.max(0.1)], "w").raw("1 J 1 j").stroke_color(a.color);
            ap.content.move_to(p).line_to(q).raw("S");
            if arrow {
                let (dx, dy) = (q.0 - p.0, q.1 - p.1);
                let len = (dx * dx + dy * dy).sqrt().max(1e-6);
                let (ux, uy) = (dx / len, dy / len);
                let spread = 0.45_f64;
                let wing = |s: f64| {
                    let (c, n) = (spread.cos(), s * spread.sin());
                    (q.0 - head * (ux * c - uy * n), q.1 - head * (uy * c + ux * n))
                };
                ap.content.move_to(wing(1.0)).line_to(q).line_to(wing(-1.0)).raw("S");
            }
            ap.extra.insert("L".into(), numbers(&[p.0, p.1, q.0, q.1]));
            ap.extra.insert(
                "LE".into(),
                PdfObject::Array(vec![name("None"), name(if arrow { "OpenArrow" } else { "None" })]),
            );
            ap.extra.insert("BS".into(), PdfObject::Dictionary(dict(vec![("W", real(width))])));
            ap
        }
        Kind::FreeText { rect, text, font_size } | Kind::Text { rect, text, font_size } => {
            let boxed = matches!(a.kind, Kind::FreeText { .. });
            let display = normal(*rect);
            let r = frame.user_rect(display);
            let size = if font_size.is_finite() { font_size.clamp(1.0, 300.0) } else { 12.0 };
            let helv = TextFont::standard("Helvetica");
            let mut ap = Appearance::new("FreeText", r);
            ap.font(&helv);
            ap.alpha(opacity, None);
            let (w, h) = (display[2] - display[0], display[3] - display[1]);
            ap.content.raw("q").cm(&frame.upright(display));
            if let Some(fill) = a.fill {
                ap.content.fill_color(fill).rect(0.0, 0.0, w, h).raw("f");
            }
            let border = if boxed { width } else { 0.0 };
            if border > 0.0 {
                ap.content.op(&[border], "w").stroke_color(a.color);
                ap.content.rect(border / 2.0, border / 2.0, w - border, h - border).raw("S");
            }
            let pad = if boxed { 2.0 + border } else { 0.0 };
            let (bytes, lossless) = win_ansi(text);
            if !lossless {
                warnings.push(unshown());
            }
            ap.content.op(&[pad, pad, (w - 2.0 * pad).max(0.0), (h - 2.0 * pad).max(0.0)], "re").raw("W n");
            ap.content.fill_color(a.color);
            let mut y = h - pad - helv.ascent * size / 1000.0;
            for line in helv.wrap(&bytes, size, (w - 2.0 * pad).max(size)) {
                font::show(&mut ap.content, "Helv", size, pad, y, &line);
                y -= size * 1.2;
            }
            ap.content.raw("Q");
            let da = format!(
                "/Helv {} Tf {} {} {} rg",
                crate::content::num(size),
                crate::content::num(a.color.0),
                crate::content::num(a.color.1),
                crate::content::num(a.color.2)
            );
            ap.extra.insert("DA".into(), PdfObject::LiteralString(da.into_bytes()));
            ap.extra.insert("BS".into(), PdfObject::Dictionary(dict(vec![("W", real(border))])));
            // The text is the annotation's contents.
            ap.extra.insert("Contents".into(), text_string(text));
            ap
        }
        Kind::Note { at, text } => {
            let display = [at[0], at[1], at[0] + 24.0, at[1] + 24.0];
            let mut ap = Appearance::new("Text", frame.user_rect(display));
            ap.alpha(opacity, None);
            ap.content.raw("q").cm(&frame.upright(display));
            ap.content.op(&[1.0], "w").raw("1 j").fill_color(a.color).op(&[0.25, 0.25, 0.25], "RG");
            ap.content.move_to((3.0, 22.0)).line_to((21.0, 22.0)).line_to((21.0, 8.0)).line_to((11.0, 8.0));
            ap.content.line_to((6.0, 3.0)).line_to((7.0, 8.0)).line_to((3.0, 8.0)).raw("h B");
            ap.content.move_to((6.5, 18.0)).line_to((17.5, 18.0)).move_to((6.5, 15.0)).line_to((17.5, 15.0));
            ap.content.move_to((6.5, 12.0)).line_to((13.5, 12.0)).raw("S Q");
            ap.extra.insert("Name".into(), name("Comment"));
            ap.extra.insert("Open".into(), PdfObject::Bool(false));
            ap.extra.insert("Contents".into(), text_string(text));
            ap
        }
        Kind::Image { rect, image } => {
            let display = normal(*rect);
            let (reference, _, _) = crate::image::add(editor, image)?;
            let mut ap = Appearance::new("Stamp", frame.user_rect(display));
            let mut xobjects = Dictionary::new();
            xobjects.insert("Im0".into(), reference);
            ap.resources.insert("XObject".into(), PdfObject::Dictionary(xobjects));
            ap.alpha(opacity, None);
            let (w, h) = (display[2] - display[0], display[3] - display[1]);
            ap.content.raw("q").cm(&frame.upright(display)).cm(&Matrix::new(w, 0.0, 0.0, h, 0.0, 0.0));
            ap.content.raw("/Im0 Do Q");
            ap.extra.insert("Name".into(), name("DHImage"));
            ap
        }
    };
    if ap.rect[2] - ap.rect[0] <= 0.0 || ap.rect[3] - ap.rect[1] <= 0.0 {
        return Err(PdfError::Structure("an annotation needs a size".into()));
    }
    let appearance = form(ap.rect, None, std::mem::take(&mut ap.resources), &ap.content);
    let appearance = editor.add(appearance);
    if a.flatten {
        return draw_on_page(editor, page_id, appearance);
    }
    let mut annot = dict(vec![
        ("Type", name("Annot")),
        ("Subtype", name(ap.subtype)),
        ("Rect", numbers(&ap.rect)),
        ("P", PdfObject::Reference(page_id)),
        ("F", PdfObject::Integer(4)),
        ("NM", unique_name()),
        ("M", now()),
        ("CreationDate", now()),
        ("C", color_array(a.color)),
        ("AP", PdfObject::Dictionary(dict(vec![("N", PdfObject::Reference(appearance))]))),
    ]);
    if opacity < 1.0 {
        annot.insert("CA".into(), real(opacity));
    }
    if !a.author.is_empty() {
        annot.insert("T".into(), text_string(&a.author));
    }
    if !a.contents.is_empty() {
        annot.insert("Contents".into(), text_string(&a.contents));
    }
    if ap.subtype == "FreeText" {
        // A text box's colour is its background, when it has one.
        annot.remove("C");
        if let Some(fill) = a.fill {
            annot.insert("C".into(), color_array(fill));
        }
    }
    annot.extend(ap.extra);
    let id = editor.add(PdfObject::Dictionary(annot));
    append(editor, page_id, id)
}

/// The warning when text had characters the standard fonts lack.
pub(crate) fn unshown() -> String {
    "Some characters are not in the standard PDF fonts and were written as \"?\".".into()
}

/// Highlight, underline, strike-out and squiggly: one quadrilateral per
/// rectangle, top-left, top-right, bottom-left, bottom-right as readers
/// expect.
fn markup(frame: &Frame, a: &Annotation, rects: &[[f64; 4]], opacity: f64) -> Result<Appearance> {
    let quads: Vec<[(f64, f64); 4]> = rects
        .iter()
        .filter(|r| r.iter().all(|v| v.is_finite()))
        .map(|r| normal(*r))
        .filter(|r| r[2] > r[0] && r[3] > r[1])
        .map(|r| [frame.user(r[0], r[1]), frame.user(r[2], r[1]), frame.user(r[0], r[3]), frame.user(r[2], r[3])])
        .collect();
    if quads.is_empty() {
        return Err(PdfError::Structure("a markup needs at least one rectangle".into()));
    }
    let subtype = match a.kind {
        Kind::Highlight { .. } => "Highlight",
        Kind::Underline { .. } => "Underline",
        Kind::StrikeOut { .. } => "StrikeOut",
        _ => "Squiggly",
    };
    let all: Vec<(f64, f64)> = quads.iter().flatten().copied().collect();
    let tallest = quads.iter().map(|q| dist(q[0], q[2])).fold(0.0, f64::max);
    let mut ap = Appearance::new(subtype, grow(bounds(&all), (tallest * 0.1).max(1.0)));
    ap.alpha(opacity, (subtype == "Highlight").then_some("Multiply"));
    for q in &quads {
        let [tl, tr, bl, br] = *q;
        let h = dist(tl, bl).max(0.01);
        // Towards the top of the line, one point long.
        let up = ((tl.0 - bl.0) / h, (tl.1 - bl.1) / h);
        let at = |p: (f64, f64), k: f64| (p.0 + up.0 * h * k, p.1 + up.1 * h * k);
        match subtype {
            "Highlight" => {
                ap.content.fill_color(a.color);
                ap.content.move_to(tl).line_to(tr).line_to(br).line_to(bl).raw("h f");
            }
            "Underline" | "StrikeOut" => {
                let k = if subtype == "Underline" { 0.08 } else { 0.42 };
                ap.content.stroke_color(a.color).op(&[(h / 14.0).max(0.5)], "w");
                ap.content.move_to(at(bl, k)).line_to(at(br, k)).raw("S");
            }
            _ => {
                let len = dist(bl, br);
                let along = ((br.0 - bl.0) / len.max(0.01), (br.1 - bl.1) / len.max(0.01));
                let step = (h / 6.0).max(1.0);
                let amp = h / 16.0;
                ap.content.stroke_color(a.color).op(&[(h / 20.0).max(0.5)], "w").raw("1 j");
                let mut t = 0.0;
                let mut first = true;
                let mut up_side = false;
                while t <= len + 1e-6 {
                    let base = at((bl.0 + along.0 * t, bl.1 + along.1 * t), 0.06);
                    let s = if up_side { amp } else { -amp };
                    let p = (base.0 + up.0 * s, base.1 + up.1 * s);
                    if first {
                        ap.content.move_to(p);
                        first = false;
                    } else {
                        ap.content.line_to(p);
                    }
                    up_side = !up_side;
                    t += step;
                }
                ap.content.raw("S");
            }
        }
    }
    let quad_points = quads.iter().flat_map(|q| q.iter().flat_map(|p| [p.0, p.1])).collect::<Vec<f64>>();
    ap.extra.insert("QuadPoints".into(), numbers(&quad_points));
    Ok(ap)
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}
