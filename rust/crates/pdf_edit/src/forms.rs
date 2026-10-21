//! Interactive forms (AcroForm): the fields a document asks to be filled,
//! and filling them, with appearances drawn for the new values so every
//! reader shows them.

use std::collections::{BTreeSet, HashMap};

use pdf_core::document::PdfDocument;
use pdf_core::error::Result;
use pdf_core::object::{Dictionary, ObjectId, PdfObject};
use pdf_text::standard14;
use pdf_text::text_state::Matrix;

use crate::annots::{rect_of, unshown};
use crate::content::{decode_text, dict, form, name, num, text_string, Content};
use crate::editor::Editor;
use crate::font::{self, win_ansi, TextFont};
use crate::frame::Frame;
use crate::model::{ChoiceOption, FieldEdit, FieldInfo, FieldKind, FormInfo, WidgetInfo};

// Field flags (ISO 32000-1 tables 221, 226, 228, 230).
const READ_ONLY: i64 = 1;
const REQUIRED: i64 = 1 << 1;
const MULTILINE: i64 = 1 << 12;
const PASSWORD: i64 = 1 << 13;
const NO_TOGGLE_TO_OFF: i64 = 1 << 14;
const RADIO: i64 = 1 << 15;
const PUSH_BUTTON: i64 = 1 << 16;
const COMBO: i64 = 1 << 17;
const EDIT: i64 = 1 << 18;
const MULTI_SELECT: i64 = 1 << 21;
const COMB: i64 = 1 << 24;

/// A terminal field: the one that holds a value, with its widgets.
struct Field {
    id: ObjectId,
    name: String,
    ft: String,
    ff: i64,
    value: Option<PdfObject>,
    da: String,
    q: i64,
    opt: Vec<ChoiceOption>,
    max_len: Option<i64>,
    tooltip: String,
    widgets: Vec<ObjectId>,
}

impl Field {
    fn kind(&self) -> FieldKind {
        match self.ft.as_str() {
            "Tx" => FieldKind::Text,
            "Ch" => FieldKind::Choice,
            "Sig" => FieldKind::Signature,
            "Btn" if self.ff & PUSH_BUTTON != 0 => FieldKind::Button,
            "Btn" if self.ff & RADIO != 0 => FieldKind::Radio,
            "Btn" => FieldKind::Checkbox,
            _ => FieldKind::Text,
        }
    }
}

/// What a field inherits from its ancestors.
#[derive(Clone, Default)]
struct Inherited {
    ft: Option<String>,
    ff: Option<i64>,
    value: Option<PdfObject>,
    da: Option<String>,
    q: Option<i64>,
    opt: Option<PdfObject>,
    max_len: Option<i64>,
}

fn string_of(doc: &PdfDocument, v: Option<&PdfObject>) -> Option<String> {
    match doc.resolve_value(v?) {
        PdfObject::LiteralString(b) | PdfObject::HexString(b) => Some(decode_text(&b)),
        PdfObject::Name(n) => Some(n),
        _ => None,
    }
}

fn acroform(doc: &PdfDocument) -> Option<&Dictionary> {
    doc.resolve_dict(doc.catalog()?.get("AcroForm")?)
}

/// Every terminal field of the document's form.
fn collect(doc: &PdfDocument) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(form) = acroform(doc) else { return out };
    let root = Inherited {
        da: string_of(doc, form.get("DA")),
        q: form.get("Q").and_then(|v| doc.resolve_value(v).as_i64()),
        ..Default::default()
    };
    let Some(PdfObject::Array(fields)) = form.get("Fields").map(|f| doc.resolve_value(f)) else {
        return out;
    };
    let mut seen = BTreeSet::new();
    for f in fields {
        if let Some(id) = f.as_ref() {
            walk(doc, id, "", &root, 0, &mut seen, &mut out);
        }
    }
    out
}

fn walk(doc: &PdfDocument, id: ObjectId, parent: &str, inherited: &Inherited, depth: usize, seen: &mut BTreeSet<ObjectId>, out: &mut Vec<Field>) {
    if depth > 32 || !seen.insert(id) {
        return;
    }
    let Some(d) = doc.resolve(id).and_then(PdfObject::as_dict) else { return };
    let partial = string_of(doc, d.get("T"));
    let name = match (&partial, parent.is_empty()) {
        (Some(p), true) => p.clone(),
        (Some(p), false) => format!("{parent}.{p}"),
        (None, _) => parent.to_owned(),
    };
    let mut inh = inherited.clone();
    if let Some(ft) = d.get("FT").and_then(|v| doc.resolve_value(v).as_name().map(str::to_owned)) {
        inh.ft = Some(ft);
    }
    if let Some(ff) = d.get("Ff").and_then(|v| doc.resolve_value(v).as_i64()) {
        inh.ff = Some(ff);
    }
    if let Some(v) = d.get("V") {
        inh.value = Some(doc.resolve_value(v));
    }
    if let Some(da) = string_of(doc, d.get("DA")) {
        inh.da = Some(da);
    }
    if let Some(q) = d.get("Q").and_then(|v| doc.resolve_value(v).as_i64()) {
        inh.q = Some(q);
    }
    if let Some(opt) = d.get("Opt") {
        inh.opt = Some(doc.resolve_value(opt));
    }
    if let Some(m) = d.get("MaxLen").and_then(|v| doc.resolve_value(v).as_i64()) {
        inh.max_len = Some(m);
    }
    let kids: Vec<ObjectId> = match d.get("Kids").map(|k| doc.resolve_value(k)) {
        Some(PdfObject::Array(items)) => items.iter().filter_map(PdfObject::as_ref).collect(),
        _ => Vec::new(),
    };
    let is_field = |k: &ObjectId| doc.resolve(*k).and_then(PdfObject::as_dict).is_some_and(|kd| kd.contains_key("T"));
    let (fields, widgets): (Vec<ObjectId>, Vec<ObjectId>) = kids.iter().partition(|k| is_field(k));
    if !fields.is_empty() {
        for kid in fields {
            walk(doc, kid, &name, &inh, depth + 1, seen, out);
        }
        return;
    }
    let widgets = if kids.is_empty() {
        let widget = d.get("Subtype").and_then(PdfObject::as_name) == Some("Widget") || d.contains_key("Rect");
        if widget { vec![id] } else { Vec::new() }
    } else {
        widgets
    };
    let opt = match &inh.opt {
        Some(PdfObject::Array(items)) => items
            .iter()
            .filter_map(|item| match doc.resolve_value(item) {
                PdfObject::Array(pair) if pair.len() >= 2 => Some(ChoiceOption {
                    value: string_of(doc, pair.first()).unwrap_or_default(),
                    label: string_of(doc, pair.get(1)).unwrap_or_default(),
                }),
                other => string_of(doc, Some(&other)).map(|s| ChoiceOption { value: s.clone(), label: s }),
            })
            .collect(),
        _ => Vec::new(),
    };
    out.push(Field {
        id,
        name,
        ft: inh.ft.clone().unwrap_or_default(),
        ff: inh.ff.unwrap_or(0),
        value: inh.value.clone(),
        da: inh.da.clone().unwrap_or_default(),
        q: inh.q.unwrap_or(0),
        opt,
        max_len: inh.max_len,
        tooltip: string_of(doc, d.get("TU")).unwrap_or_default(),
        widgets,
    });
}

/// Which page each annotation is on.
fn pages_of_annots(doc: &PdfDocument) -> HashMap<ObjectId, (usize, ObjectId)> {
    let mut map = HashMap::new();
    for (index, page) in doc.collect_page_ids().unwrap_or_default().into_iter().enumerate() {
        let Some(d) = doc.resolve(page).and_then(PdfObject::as_dict) else { continue };
        if let Some(PdfObject::Array(items)) = d.get("Annots").map(|a| doc.resolve_value(a)) {
            for item in items {
                if let Some(id) = item.as_ref() {
                    map.entry(id).or_insert((index, page));
                }
            }
        }
    }
    map
}

/// The states a button widget can show, besides Off.
fn on_states(doc: &PdfDocument, widget: &Dictionary) -> Vec<String> {
    let normal = widget
        .get("AP")
        .and_then(|ap| doc.resolve_dict(ap))
        .and_then(|ap| ap.get("N"))
        .and_then(|n| doc.resolve_value(n).as_dict().cloned());
    match normal {
        Some(n) if !n.contains_key("BBox") => n.keys().filter(|k| *k != "Off").cloned().collect(),
        _ => Vec::new(),
    }
}

/// The font name, size and colour operators a /DA string sets.
fn parse_da(da: &str) -> (Option<String>, f64, String) {
    let (mut font, mut size, mut color): (Option<String>, f64, String) = (None, 0.0, String::new());
    let mut stack: Vec<&str> = Vec::new();
    for token in da.split_whitespace() {
        match token {
            "Tf" if stack.len() >= 2 => {
                font = stack[stack.len() - 2].strip_prefix('/').map(str::to_owned);
                size = stack[stack.len() - 1].parse().unwrap_or(0.0);
                stack.clear();
            }
            "g" | "rg" | "k" => {
                let n = match token {
                    "g" => 1,
                    "rg" => 3,
                    _ => 4,
                };
                if stack.len() >= n {
                    color = format!("{} {token}", stack[stack.len() - n..].join(" "));
                }
                stack.clear();
            }
            t if t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '\'' || c == '"') => stack.clear(),
            t => stack.push(t),
        }
    }
    (font, if size.is_finite() { size.max(0.0) } else { 0.0 }, color)
}

/// The form's fields, for the app to show.
pub fn fields(doc: &PdfDocument) -> FormInfo {
    let all = collect(doc);
    let pages = pages_of_annots(doc);
    let page_ids = doc.collect_page_ids().unwrap_or_default();
    let mut info = FormInfo {
        xfa: acroform(doc).is_some_and(|f| f.contains_key("XFA")),
        ..Default::default()
    };
    for field in &all {
        let kind = field.kind();
        let value = match &field.value {
            Some(PdfObject::Array(items)) => items.first().and_then(|v| string_of(doc, Some(v))).unwrap_or_default(),
            Some(v) => string_of(doc, Some(v)).unwrap_or_default(),
            None => String::new(),
        };
        if kind == FieldKind::Signature && field.value.is_some() {
            info.signed = true;
        }
        let values = match &field.value {
            Some(PdfObject::Array(items)) => items.iter().filter_map(|v| string_of(doc, Some(v))).collect(),
            _ if !value.is_empty() && kind == FieldKind::Choice => vec![value.clone()],
            _ => Vec::new(),
        };
        let mut states: Vec<String> = Vec::new();
        let mut widgets = Vec::new();
        for w in &field.widgets {
            let Some(wd) = doc.resolve(*w).and_then(PdfObject::as_dict) else { continue };
            let located = pages.get(w).copied().or_else(|| {
                let p = wd.get("P")?.as_ref()?;
                page_ids.iter().position(|id| *id == p).map(|i| (i, p))
            });
            let Some((index, page)) = located else { continue };
            let Some(rect) = rect_of(doc, wd.get("Rect")) else { continue };
            let Ok(frame) = Frame::of(doc, page) else { continue };
            let own = on_states(doc, wd);
            for s in &own {
                if !states.contains(s) {
                    states.push(s.clone());
                }
            }
            let da = string_of(doc, wd.get("DA")).unwrap_or_else(|| field.da.clone());
            widgets.push(WidgetInfo {
                page: index as u32 + 1,
                rect: frame.display_rect(rect),
                on_state: matches!(kind, FieldKind::Checkbox | FieldKind::Radio).then(|| own.first().cloned()).flatten(),
                font_size: parse_da(&da).1,
            });
        }
        let value = match kind {
            FieldKind::Checkbox | FieldKind::Radio if value.is_empty() => "Off".into(),
            // Line breaks as the app writes them.
            FieldKind::Text => value.replace("\r\n", "\n").replace('\r', "\n"),
            _ => value,
        };
        info.fields.push(FieldInfo {
            name: field.name.clone(),
            kind,
            value,
            values,
            options: field.opt.clone(),
            states,
            read_only: field.ff & READ_ONLY != 0,
            required: field.ff & REQUIRED != 0,
            multiline: kind == FieldKind::Text && field.ff & MULTILINE != 0,
            password: kind == FieldKind::Text && field.ff & PASSWORD != 0,
            comb: kind == FieldKind::Text && field.ff & COMB != 0 && field.max_len.is_some(),
            combo: kind == FieldKind::Choice && field.ff & COMBO != 0,
            editable: kind == FieldKind::Choice && field.ff & EDIT != 0,
            multi_select: kind == FieldKind::Choice && field.ff & MULTI_SELECT != 0,
            max_length: field.max_len.and_then(|m| u32::try_from(m).ok()),
            tooltip: field.tooltip.clone(),
            widgets,
        });
    }
    info
}

/// Fill fields. Fields that cannot take a value (read-only ones, buttons,
/// signatures) are left as they are, with a warning.
pub fn fill(editor: &mut Editor, edits: &[FieldEdit], warnings: &mut Vec<String>) -> Result<()> {
    let all = collect(&editor.doc);
    let mut changed = false;
    for edit in edits {
        let Some(field) = all.iter().find(|f| f.name == edit.name) else {
            warnings.push(format!("The form has no field called \"{}\".", edit.name));
            continue;
        };
        if field.ff & READ_ONLY != 0 {
            warnings.push(format!("\"{}\" is read-only and was left as it was.", field.name));
            continue;
        }
        match field.kind() {
            FieldKind::Text => fill_text(editor, field, edit.value.as_deref().unwrap_or(""), warnings)?,
            FieldKind::Checkbox | FieldKind::Radio => fill_button(editor, field, edit)?,
            FieldKind::Choice => fill_choice(editor, field, edit, warnings)?,
            FieldKind::Signature | FieldKind::Button => {
                warnings.push(format!("\"{}\" cannot be filled in.", field.name));
                continue;
            }
        }
        changed = true;
    }
    if changed {
        drop_xfa(editor);
    }
    Ok(())
}

/// Readers that know XFA show its data rather than the fields', so a
/// filled form drops it.
fn drop_xfa(editor: &mut Editor) {
    let Some(root) = editor.doc.root_ref() else { return };
    let Some(mut catalog) = editor.dict(root) else { return };
    match catalog.get("AcroForm").cloned() {
        Some(PdfObject::Reference(id)) => {
            if let Some(mut form) = editor.dict(id) {
                if form.remove("XFA").is_some() {
                    editor.set(id, PdfObject::Dictionary(form));
                }
            }
        }
        Some(PdfObject::Dictionary(mut form)) => {
            if form.remove("XFA").is_some() {
                catalog.insert("AcroForm".into(), PdfObject::Dictionary(form));
                editor.set(root, PdfObject::Dictionary(catalog));
            }
        }
        _ => {}
    }
}

fn set_entry(editor: &mut Editor, id: ObjectId, key: &str, value: Option<PdfObject>) {
    let Some(mut d) = editor.dict(id) else { return };
    match value {
        Some(v) => {
            d.insert(key.into(), v);
        }
        None => {
            d.remove(key);
        }
    }
    editor.set(id, PdfObject::Dictionary(d));
}

fn fill_text(editor: &mut Editor, field: &Field, value: &str, warnings: &mut Vec<String>) -> Result<()> {
    let mut value: String = value.chars().filter(|c| *c != '\r').collect();
    if field.ff & MULTILINE == 0 {
        value = value.replace('\n', " ");
    }
    if let Some(max) = field.max_len.filter(|m| *m > 0) {
        value = value.chars().take(max as usize).collect();
    }
    set_entry(editor, field.id, "V", Some(text_string(&value)));
    let shown = if field.ff & PASSWORD != 0 { "*".repeat(value.chars().count()) } else { value };
    let lines = Lines::Text { text: shown, multiline: field.ff & MULTILINE != 0, comb: (field.ff & COMB != 0).then_some(field.max_len.unwrap_or(0)).filter(|m| *m > 0) };
    for widget in &field.widgets {
        draw_widget(editor, field, *widget, &lines, warnings)?;
    }
    Ok(())
}

fn fill_choice(editor: &mut Editor, field: &Field, edit: &FieldEdit, warnings: &mut Vec<String>) -> Result<()> {
    let selected: Vec<String> = match (&edit.selected, &edit.value) {
        (Some(list), _) => list.clone(),
        (None, Some(v)) if !v.is_empty() => vec![v.clone()],
        _ => Vec::new(),
    };
    let selected: Vec<String> = if field.ff & MULTI_SELECT != 0 { selected } else { selected.into_iter().take(1).collect() };
    let value = match selected.len() {
        0 => None,
        1 => Some(text_string(&selected[0])),
        _ => Some(PdfObject::Array(selected.iter().map(|s| text_string(s)).collect())),
    };
    set_entry(editor, field.id, "V", value);
    let indices: Vec<usize> = field
        .opt
        .iter()
        .enumerate()
        .filter(|(_, o)| selected.contains(&o.value))
        .map(|(i, _)| i)
        .collect();
    let combo = field.ff & COMBO != 0;
    let index_value = (!combo && !indices.is_empty()).then(|| PdfObject::Array(indices.iter().map(|i| PdfObject::Integer(*i as i64)).collect()));
    set_entry(editor, field.id, "I", index_value);
    let label = |v: &String| field.opt.iter().find(|o| &o.value == v).map(|o| o.label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| v.clone());
    let lines = if combo {
        Lines::Text { text: selected.first().map(label).unwrap_or_default(), multiline: false, comb: None }
    } else {
        Lines::List {
            items: field.opt.iter().map(|o| if o.label.is_empty() { o.value.clone() } else { o.label.clone() }).collect(),
            chosen: indices,
        }
    };
    for widget in &field.widgets {
        draw_widget(editor, field, *widget, &lines, warnings)?;
    }
    Ok(())
}

fn fill_button(editor: &mut Editor, field: &Field, edit: &FieldEdit) -> Result<()> {
    let radio = field.ff & RADIO != 0;
    // The state asked for: a radio button's or check box's own state
    // name, or for a check box "checked".
    let wanted: Option<String> = match (&edit.value, edit.checked) {
        (Some(v), _) if v != "Off" && !v.is_empty() => Some(v.clone()),
        (Some(_), _) => None,
        (None, Some(true)) => Some(String::new()),
        _ => None,
    };
    if radio && wanted.is_none() && field.ff & NO_TOGGLE_TO_OFF != 0 && edit.value.is_none() {
        // A group that must keep one button on.
        return Ok(());
    }
    let mut value_state: Option<String> = None;
    for widget in &field.widgets {
        let Some(wd) = editor.dict(*widget) else { continue };
        let mut states = on_states(&editor.doc, &wd);
        if states.is_empty() && !radio {
            // A check box without appearances: draw them.
            states = vec![add_check_appearance(editor, field, *widget)?];
        }
        let state = match &wanted {
            Some(w) if w.is_empty() => states.first().cloned(),
            // A state's own name, or an export value from /Opt, whose
            // states are named by index.
            Some(w) => states.iter().find(|s| *s == w).cloned().or_else(|| {
                let index = field.opt.iter().position(|o| &o.value == w)?.to_string();
                states.iter().find(|s| **s == index).cloned()
            }),
            None => None,
        };
        if let Some(s) = &state {
            value_state.get_or_insert_with(|| s.clone());
        }
        set_entry(editor, *widget, "AS", Some(name(state.as_deref().unwrap_or("Off"))));
    }
    let v = match (&value_state, &wanted) {
        (Some(s), _) => s.clone(),
        // A state no widget draws, for a group whose widgets live elsewhere.
        (None, Some(w)) if !w.is_empty() => w.clone(),
        _ => "Off".into(),
    };
    set_entry(editor, field.id, "V", Some(name(&v)));
    Ok(())
}

/// Appearances for a check box that has none: a ZapfDingbats tick when on.
fn add_check_appearance(editor: &mut Editor, field: &Field, widget: ObjectId) -> Result<String> {
    let wd = editor.dict(widget).unwrap_or_default();
    let shape = widget_shape(&editor.doc, &wd);
    let (_, size, color) = parse_da(&string_of(&editor.doc, wd.get("DA")).unwrap_or_else(|| field.da.clone()));
    let mk = wd.get("MK").and_then(|m| editor.doc.resolve_dict(m)).cloned().unwrap_or_default();
    let glyph = string_of(&editor.doc, mk.get("CA")).and_then(|s| s.bytes().next()).unwrap_or(b'4');
    let dingbats = standard14::lookup("ZapfDingbats");
    let advance = dingbats.and_then(|f| f.builtin_name(glyph).and_then(|n| f.width(n))).unwrap_or(800.0) / 1000.0;
    let (w, h) = (shape.w, shape.h);
    let size = if size > 0.0 { size } else { (w.min(h) * 0.8).max(1.0) };
    let mut fonts = Dictionary::new();
    fonts.insert("ZaDb".into(), TextFont::standard("ZapfDingbats").dict());
    let resources = dict(vec![("Font", PdfObject::Dictionary(fonts))]);
    let mut on = Content::new();
    frame_of_widget(&mut on, &editor.doc, &mk, &wd, &shape);
    on.raw("q");
    on.raw(if color.is_empty() { "0 g" } else { &color });
    font::show(&mut on, "ZaDb", size, (w - advance * size) / 2.0, (h - size * 0.7) / 2.0, &[glyph]);
    on.raw("Q");
    let mut off = Content::new();
    frame_of_widget(&mut off, &editor.doc, &mk, &wd, &shape);
    let on_form = editor.add(form([0.0, 0.0, w, h], shape.matrix.as_ref(), resources, &on));
    let off_form = editor.add(form([0.0, 0.0, w, h], shape.matrix.as_ref(), Dictionary::new(), &off));
    let normal = dict(vec![("Yes", PdfObject::Reference(on_form)), ("Off", PdfObject::Reference(off_form))]);
    set_entry(editor, widget, "AP", Some(PdfObject::Dictionary(dict(vec![("N", PdfObject::Dictionary(normal))]))));
    Ok("Yes".into())
}

/// A widget's box as its appearance sees it: width and height after its
/// rotation (/MK /R), and the matrix that turns the appearance.
struct Shape {
    w: f64,
    h: f64,
    matrix: Option<Matrix>,
}

fn widget_shape(doc: &PdfDocument, wd: &Dictionary) -> Shape {
    let r = rect_of(doc, wd.get("Rect")).unwrap_or([0.0, 0.0, 0.0, 0.0]);
    let (w, h) = ((r[2] - r[0]).max(1.0), (r[3] - r[1]).max(1.0));
    let turn = wd
        .get("MK")
        .and_then(|m| doc.resolve_dict(m))
        .and_then(|m| m.get("R"))
        .and_then(|r| doc.resolve_value(r).as_i64())
        .unwrap_or(0)
        .rem_euclid(360);
    match turn {
        90 => Shape { w: h, h: w, matrix: Some(Matrix::new(0.0, 1.0, -1.0, 0.0, 0.0, 0.0)) },
        180 => Shape { w, h, matrix: Some(Matrix::new(-1.0, 0.0, 0.0, -1.0, 0.0, 0.0)) },
        270 => Shape { w: h, h: w, matrix: Some(Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, 0.0)) },
        _ => Shape { w, h, matrix: None },
    }
}

/// A colour array from /MK as a fill (or stroke) operator.
fn color_op(doc: &PdfDocument, v: Option<&PdfObject>, stroke: bool) -> Option<String> {
    let PdfObject::Array(parts) = doc.resolve_value(v?) else { return None };
    let n: Vec<String> = parts
        .iter()
        .filter_map(|p| match doc.resolve_value(p) {
            PdfObject::Integer(i) => Some(i as f64),
            PdfObject::Real(r) => Some(r),
            _ => None,
        })
        .map(num)
        .collect();
    let op = match (n.len(), stroke) {
        (1, false) => "g",
        (1, true) => "G",
        (3, false) => "rg",
        (3, true) => "RG",
        (4, false) => "k",
        (4, true) => "K",
        _ => return None,
    };
    Some(format!("{} {op}", n.join(" ")))
}

/// The widget's background and border, as /MK asks; the border's width.
fn frame_of_widget(content: &mut Content, doc: &PdfDocument, mk: &Dictionary, wd: &Dictionary, shape: &Shape) -> f64 {
    if let Some(bg) = color_op(doc, mk.get("BG"), false) {
        content.raw(&bg).rect(0.0, 0.0, shape.w, shape.h).raw("f");
    }
    let Some(bc) = color_op(doc, mk.get("BC"), true) else { return 0.0 };
    let bw = wd
        .get("BS")
        .and_then(|b| doc.resolve_dict(b))
        .and_then(|b| b.get("W"))
        .and_then(|w| match doc.resolve_value(w) {
            PdfObject::Integer(i) => Some(i as f64),
            PdfObject::Real(r) => Some(r),
            _ => None,
        })
        .unwrap_or(1.0);
    if bw > 0.0 {
        content.raw(&bc).op(&[bw], "w").rect(bw / 2.0, bw / 2.0, shape.w - bw, shape.h - bw).raw("S");
    }
    bw
}

/// What a widget shows.
enum Lines {
    Text { text: String, multiline: bool, comb: Option<i64> },
    List { items: Vec<String>, chosen: Vec<usize> },
}

/// The font to set a widget's text in: the form's own when it is a
/// standard, unembedded font with a known encoding; Helvetica otherwise.
fn widget_font(doc: &PdfDocument, da_font: Option<&str>, text: &str) -> (String, PdfObject, TextFont) {
    let helvetica = TextFont::standard("Helvetica");
    let ours = ("DHHelv".to_owned(), helvetica.dict(), helvetica.clone());
    let Some(da_font) = da_font else { return ours };
    let form_font = acroform(doc)
        .and_then(|f| f.get("DR"))
        .and_then(|dr| doc.resolve_dict(dr))
        .and_then(|dr| dr.get("Font"))
        .and_then(|fonts| doc.resolve_dict(fonts))
        .and_then(|fonts| fonts.get(da_font).cloned());
    let Some(reference) = form_font else { return ours };
    let Some(fd) = doc.resolve_dict(&reference) else { return ours };
    let subtype = fd.get("Subtype").and_then(PdfObject::as_name).unwrap_or("");
    if !matches!(subtype, "Type1" | "TrueType" | "MMType1") {
        return ours;
    }
    let embedded = fd
        .get("FontDescriptor")
        .and_then(|d| doc.resolve_dict(d))
        .is_some_and(|d| ["FontFile", "FontFile2", "FontFile3"].iter().any(|k| d.contains_key(*k)));
    let encoding = fd.get("Encoding").map(|e| doc.resolve_value(e));
    let ascii = text.chars().all(|c| (' '..='~').contains(&c) && c != '\'' && c != '`');
    let encoded = match encoding {
        Some(PdfObject::Name(n)) => n == "WinAnsiEncoding",
        None => ascii,
        _ => false,
    };
    let base = fd.get("BaseFont").and_then(PdfObject::as_name).unwrap_or("");
    if embedded || !encoded || base.is_empty() {
        return ours;
    }
    let metrics = if standard14::lookup(base).is_some() {
        TextFont::standard(base)
    } else {
        let first = fd.get("FirstChar").and_then(|v| doc.resolve_value(v).as_i64()).unwrap_or(0).clamp(0, 255) as usize;
        let widths: Vec<f64> = match fd.get("Widths").map(|w| doc.resolve_value(w)) {
            Some(PdfObject::Array(items)) => items
                .iter()
                .map(|v| match doc.resolve_value(v) {
                    PdfObject::Integer(i) => i as f64,
                    PdfObject::Real(r) => r,
                    _ => 0.0,
                })
                .collect(),
            _ => Vec::new(),
        };
        TextFont::with_widths(base, first, &widths, &helvetica)
    };
    (da_font.to_owned(), reference, metrics)
}

/// Draw a text, drop-down or list widget's appearance for its new value.
fn draw_widget(editor: &mut Editor, field: &Field, widget: ObjectId, lines: &Lines, warnings: &mut Vec<String>) -> Result<()> {
    let Some(wd) = editor.dict(widget) else { return Ok(()) };
    let doc = &editor.doc;
    let shape = widget_shape(doc, &wd);
    let mk = wd.get("MK").and_then(|m| doc.resolve_dict(m)).cloned().unwrap_or_default();
    let da = string_of(doc, wd.get("DA")).unwrap_or_else(|| field.da.clone());
    let (da_font, da_size, color) = parse_da(&da);
    let all_text = match lines {
        Lines::Text { text, .. } => text.clone(),
        Lines::List { items, .. } => items.join("\n"),
    };
    let (resource, font_ref, font) = widget_font(doc, da_font.as_deref(), &all_text);
    let q = wd.get("Q").and_then(|v| doc.resolve_value(v).as_i64()).unwrap_or(field.q);
    let mut content = Content::new();
    let bw = frame_of_widget(&mut content, doc, &mk, &wd, &shape);
    let (w, h) = (shape.w, shape.h);
    let pad = bw.max(1.0) + 1.0;
    let (inner_w, inner_h) = ((w - 2.0 * pad).max(1.0), (h - 2.0 * pad).max(1.0));
    let color = if color.is_empty() { "0 g".to_owned() } else { color };
    content.raw("/Tx BMC").raw("q");
    content.rect(bw, bw, (w - 2.0 * bw).max(0.0), (h - 2.0 * bw).max(0.0)).raw("W n");
    let mut lossy = false;
    let x_for = |width: f64| match q {
        1 => (w - width) / 2.0,
        2 => w - pad - width,
        _ => pad,
    };
    let asc = font.ascent / 1000.0;
    let desc = font.descent / 1000.0;
    match lines {
        Lines::Text { text, multiline: false, comb } => {
            let (bytes, ok) = win_ansi(text);
            lossy |= !ok;
            let size = if da_size > 0.0 {
                da_size
            } else {
                let by_height = inner_h / (asc - desc).max(0.5);
                let one = font.width(&bytes, 1.0);
                let by_width = if one > 0.0 { inner_w / one } else { by_height };
                by_height.min(by_width).clamp(4.0, 12.0)
            };
            let y = (h - (asc - desc) * size) / 2.0 - desc * size;
            content.raw(&color);
            if let Some(cells) = comb {
                let cell = w / *cells as f64;
                for (i, b) in bytes.iter().enumerate() {
                    let cw = font.width(&[*b], size);
                    font::show(&mut content, &resource, size, cell * i as f64 + (cell - cw) / 2.0, y, &[*b]);
                }
            } else {
                let x = x_for(font.width(&bytes, size));
                font::show(&mut content, &resource, size, x, y, &bytes);
            }
        }
        Lines::Text { text, multiline: true, .. } => {
            let (bytes, ok) = win_ansi(text);
            lossy |= !ok;
            let mut size = if da_size > 0.0 { da_size } else { 12.0 };
            let mut wrapped = font.wrap(&bytes, size, inner_w);
            if da_size <= 0.0 {
                while size > 4.0 && wrapped.len() as f64 * size * 1.15 > inner_h {
                    size -= 0.5;
                    wrapped = font.wrap(&bytes, size, inner_w);
                }
            }
            content.raw(&color);
            let mut y = h - pad - asc * size;
            for line in wrapped {
                let x = x_for(font.width(&line, size));
                font::show(&mut content, &resource, size, x, y, &line);
                y -= size * 1.15;
            }
        }
        Lines::List { items, chosen } => {
            let size = if da_size > 0.0 { da_size } else { 12.0 };
            let step = size * 1.15;
            let mut y = h - pad;
            for (i, item) in items.iter().enumerate() {
                if y - step < -step {
                    break;
                }
                if chosen.contains(&i) {
                    content.raw("0.6 0.75 0.85 rg").rect(bw, y - step, w - 2.0 * bw, step).raw("f");
                }
                let (bytes, ok) = win_ansi(item);
                lossy |= !ok;
                content.raw(&color);
                font::show(&mut content, &resource, size, pad, y - asc * size - (step - (asc - desc) * size) / 2.0, &bytes);
                y -= step;
            }
        }
    }
    content.raw("Q").raw("EMC");
    if lossy {
        warnings.push(unshown());
    }
    let mut fonts = Dictionary::new();
    fonts.insert(resource, font_ref);
    let resources = dict(vec![("Font", PdfObject::Dictionary(fonts))]);
    let appearance = form([0.0, 0.0, w, h], shape.matrix.as_ref(), resources, &content);
    let appearance = editor.add(appearance);
    set_entry(editor, widget, "AP", Some(PdfObject::Dictionary(dict(vec![("N", PdfObject::Reference(appearance))]))));
    Ok(())
}
