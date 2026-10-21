//! What the app asks for and what it is told, as they cross to Dart in
//! JSON. Positions are displayed page points, top-left origin, and
//! rectangles are `[left, top, right, bottom]`; pages are 1-based.

use serde::{Deserialize, Serialize};

/// An sRGB colour, each part 0 to 1; written as `"#RRGGBB"`.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct Color(pub f64, pub f64, pub f64);

impl Color {
    pub const BLACK: Color = Color(0.0, 0.0, 0.0);

    pub fn hex(&self) -> String {
        let part = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02X}{:02X}{:02X}", part(self.0), part(self.1), part(self.2))
    }
}

impl TryFrom<String> for Color {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let hex = text.trim().trim_start_matches('#');
        // #AARRGGBB, as Flutter writes a colour, keeps its RGB.
        let hex = if hex.len() == 8 { &hex[2..] } else { hex };
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("bad colour {text:?}"));
        }
        let part = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
        Ok(Color(part(0), part(2), part(4)))
    }
}

/// Everything one save changes.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Edits {
    /// Form fields to fill, by full name.
    pub fields: Vec<FieldEdit>,
    pub annotations: Vec<Annotation>,
    /// Annotations to remove.
    pub delete: Vec<Delete>,
    /// Text or pictures drawn onto pages: page numbers, headers and
    /// footers, watermarks.
    pub marks: Vec<Mark>,
}

/// A field's new value: `value` for text, a choice or a radio button's
/// state; `checked` for a check box; `selected` for a list that takes
/// several.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FieldEdit {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub selected: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub page: u32,
    /// "number generation", as [`AnnotInfo::id`] gives it.
    pub id: String,
}

fn black() -> Color {
    Color::BLACK
}

fn one() -> f64 {
    1.0
}

fn twelve() -> f64 {
    12.0
}

/// One annotation to add. With `flatten` it is drawn into the page
/// instead, where it can no longer be moved or removed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    pub page: u32,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default = "black")]
    pub color: Color,
    /// Inside colour for rectangles and ellipses, background for text.
    #[serde(default)]
    pub fill: Option<Color>,
    /// Line width in points.
    #[serde(default = "one")]
    pub width: f64,
    #[serde(default = "one")]
    pub opacity: f64,
    /// The comment shown with a markup.
    #[serde(default)]
    pub contents: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub flatten: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Kind {
    /// Text markups, one rectangle per line of the selection.
    Highlight { rects: Vec<[f64; 4]> },
    Underline { rects: Vec<[f64; 4]> },
    StrikeOut { rects: Vec<[f64; 4]> },
    Squiggly { rects: Vec<[f64; 4]> },
    /// Freehand drawing.
    Ink { strokes: Vec<Vec<[f64; 2]>> },
    Rectangle { rect: [f64; 4] },
    Ellipse { rect: [f64; 4] },
    Line { from: [f64; 2], to: [f64; 2] },
    Arrow { from: [f64; 2], to: [f64; 2] },
    /// A text box with a border.
    #[serde(rename_all = "camelCase")]
    FreeText {
        rect: [f64; 4],
        text: String,
        #[serde(default = "twelve")]
        font_size: f64,
    },
    /// Typed text without a border, as when filling in a form that has
    /// no fields.
    #[serde(rename_all = "camelCase")]
    Text {
        rect: [f64; 4],
        text: String,
        #[serde(default = "twelve")]
        font_size: f64,
    },
    /// A sticky note whose icon's top-left corner is `at`.
    Note { at: [f64; 2], text: String },
    /// A picture: a photo, a scanned signature, a stamp.
    Image { rect: [f64; 4], image: ImageSource },
    /// A drawn signature, its strokes in page points.
    Signature { rect: [f64; 4], strokes: Vec<Vec<[f64; 2]>> },
}

/// A picture's file: raw RGBA when `width` and `height` are given
/// (straight alpha, as Flutter's `rawStraightRgba` gives it), otherwise a
/// JPEG.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageSource {
    pub path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Position {
    TopLeft,
    TopCenter,
    TopRight,
    Center,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FontChoice {
    #[default]
    Sans,
    Serif,
    Mono,
}

/// Text or a picture drawn onto many pages. In `text`, `{page}` is the
/// page's number and `{pages}` the count.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mark {
    /// 1-based; empty means every page.
    #[serde(default)]
    pub pages: Vec<u32>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub image: Option<ImageSource>,
    /// The picture's size in points; its own proportions when absent.
    #[serde(default)]
    pub size: Option<[f64; 2]>,
    pub position: Position,
    #[serde(default = "twelve")]
    pub font_size: f64,
    #[serde(default)]
    pub font: FontChoice,
    #[serde(default)]
    pub bold: bool,
    #[serde(default = "black")]
    pub color: Color,
    #[serde(default = "one")]
    pub opacity: f64,
    /// Degrees, anticlockwise as the page is shown.
    #[serde(default)]
    pub rotation: f64,
    /// Distance from the page's edges in points.
    #[serde(default = "margin")]
    pub margin: f64,
}

fn margin() -> f64 {
    28.0
}

/// A document's form.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormInfo {
    pub fields: Vec<FieldInfo>,
    /// The form also has an XFA description, which filling removes so
    /// readers show the values written here.
    pub xfa: bool,
    /// Some signature field is signed; an incremental save keeps it valid.
    pub signed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldKind {
    Text,
    Checkbox,
    Radio,
    Choice,
    Signature,
    Button,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldInfo {
    /// The full name, parts joined with ".".
    pub name: String,
    pub kind: FieldKind,
    /// Text, the chosen option, or a button's state ("Off" when clear).
    pub value: String,
    /// Every chosen option of a list that takes several.
    pub values: Vec<String>,
    pub options: Vec<ChoiceOption>,
    /// The states a check box or radio group can be in, besides "Off".
    pub states: Vec<String>,
    pub read_only: bool,
    pub required: bool,
    pub multiline: bool,
    pub password: bool,
    pub comb: bool,
    /// A drop-down rather than a list.
    pub combo: bool,
    /// A drop-down that also takes typed text.
    pub editable: bool,
    pub multi_select: bool,
    pub max_length: Option<u32>,
    pub tooltip: String,
    pub widgets: Vec<WidgetInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChoiceOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetInfo {
    pub page: u32,
    pub rect: [f64; 4],
    /// The state this check box or radio button stands for.
    pub on_state: Option<String>,
    /// The text size the form asks for; 0 means fit the box.
    pub font_size: f64,
}

/// An annotation already on a page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotInfo {
    pub id: String,
    pub subtype: String,
    pub rect: [f64; 4],
    pub contents: String,
    pub author: String,
    pub color: Option<String>,
    /// Added by this app.
    pub ours: bool,
}
