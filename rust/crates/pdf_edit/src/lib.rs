//! Editing PDFs the way a reader app does: fill forms, sign, annotate,
//! stamp pages, and save so the original stays intact.
//!
//! * Saving appends an incremental update: the original bytes are kept
//!   as they are, so signatures stay valid and nothing the engine does
//!   not understand is lost. A file whose index had to be rebuilt is
//!   written out whole instead. Encrypted files keep their password.
//! * Every annotation is written with an appearance stream, so any reader
//!   shows it as this app drew it.
//! * Positions are displayed page points with a top-left origin, as the
//!   text layout and the viewer use; they are mapped through the page's
//!   CropBox and /Rotate here.

mod annots;
mod content;
mod editor;
mod font;
mod forms;
mod frame;
mod image;
mod marks;
pub mod model;

pub use annots::annotations;
pub use editor::Editor;
pub use forms::fields;
pub use model::*;

use pdf_core::error::{PdfError, Result};

/// Apply edits to a PDF's bytes; the result is the new file and what the
/// caller should know (characters a font could not show, fields that
/// were left as they are).
pub fn apply(bytes: Vec<u8>, password: &str, edits: &Edits) -> Result<(Vec<u8>, Vec<String>)> {
    let mut editor = Editor::open(bytes, password)?;
    let warnings = apply_to(&mut editor, edits)?;
    Ok((editor.save()?, warnings))
}

/// [`apply`] on an open editor.
pub fn apply_to(editor: &mut Editor, edits: &Edits) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let pages = editor.doc.collect_page_ids().unwrap_or_default();
    let page = |number: u32| -> Result<pdf_core::object::ObjectId> {
        let index = (number as usize).checked_sub(1).ok_or(PdfError::PageIndex(0))?;
        pages.get(index).copied().ok_or(PdfError::PageIndex(index))
    };
    for delete in &edits.delete {
        annots::remove(editor, page(delete.page)?, &delete.id)?;
    }
    if !edits.fields.is_empty() {
        forms::fill(editor, &edits.fields, &mut warnings)?;
    }
    for annotation in &edits.annotations {
        annots::add(editor, page(annotation.page)?, annotation, &mut warnings)?;
    }
    for mark in &edits.marks {
        let chosen: Vec<usize> = if mark.pages.is_empty() {
            (0..pages.len()).collect()
        } else {
            let mut chosen = Vec::new();
            for number in &mark.pages {
                page(*number)?;
                chosen.push(*number as usize - 1);
            }
            chosen
        };
        marks::add(editor, &pages, &chosen, mark, &mut warnings)?;
    }
    warnings.dedup();
    Ok(warnings)
}

#[cfg(test)]
mod tests;
