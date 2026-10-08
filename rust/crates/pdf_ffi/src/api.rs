//! Milestone 11: a hand-written C ABI for Flutter's `dart:ffi`.
//!
//! Conventions
//! -----------
//! * All strings are UTF-8, NUL-terminated C strings.
//! * Functions returning `*mut c_char` give ownership to the caller —
//!   release with `pdf_free_string`. They return NULL on failure.
//! * Functions returning `i32` use 0 for success, -1 for failure.
//! * On failure, `pdf_last_error()` returns a (borrowed) message valid
//!   until the next call on the same thread.
//! * Page selections are 1-based range strings: `"1-3,5,9"`. An empty
//!   string means "all pages".

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::SystemTime;

use pdf_ai::chunker::ChunkOptions;
use pdf_core::crypt::encrypt_to_bytes;
use pdf_core::document::PdfDocument;
use pdf_core::error::PdfError;
use pdf_ops::compose::{images_to_document, ComposeOptions, PageFit};

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
    /// Set by the most recent render on this thread. A render that succeeded
    /// but had to leave something out — an image codec this build cannot
    /// read, a font the document did not embed — says so here, so the caller
    /// can tell an exact page from an approximate one.
    static LAST_WARNINGS: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

fn set_warnings(warnings: &[String]) {
    let joined = warnings.join("\n").replace('\0', " ");
    LAST_WARNINGS.with(|slot| {
        *slot.borrow_mut() = CString::new(joined).unwrap_or_default();
    });
}

fn set_error(message: impl Into<String>) {
    let message = message.into().replace('\0', " ");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(message).unwrap_or_default();
    });
}

fn error_code(err: &PdfError) -> &'static str {
    match err {
        PdfError::Encrypted => "ENCRYPTED",
        PdfError::WrongPassword => "WRONG_PASSWORD",
        PdfError::MissingHeader => "NOT_A_PDF",
        PdfError::PageIndex(_) => "PAGE_OUT_OF_RANGE",
        _ => "ERROR",
    }
}

fn set_pdf_error(err: &PdfError) {
    set_error(format!("{}: {}", error_code(err), err));
}

fn non_negative_page(page: c_int) -> Result<usize, ()> {
    usize::try_from(page).map_err(|_| {
        set_error(format!(
            "PAGE_OUT_OF_RANGE: page index {page} is out of bounds"
        ));
    })
}

unsafe fn cstr<'a>(ptr: *const c_char) -> Result<&'a str, ()> {
    if ptr.is_null() {
        set_error("ERROR: null argument");
        return Err(());
    }
    CStr::from_ptr(ptr).to_str().map_err(|_| {
        set_error("ERROR: argument is not valid UTF-8");
    })
}

fn to_c_string(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

fn run_str(f: impl FnOnce() -> Result<String, PdfError>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => to_c_string(value),
        Ok(Err(err)) => {
            set_pdf_error(&err);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error("PANIC: internal error");
            std::ptr::null_mut()
        }
    }
}

fn run_int(f: impl FnOnce() -> Result<i64, PdfError>) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value.try_into().unwrap_or(c_int::MAX),
        Ok(Err(err)) => {
            set_pdf_error(&err);
            -1
        }
        Err(_) => {
            set_error("PANIC: internal error");
            -1
        }
    }
}

fn open(path: &str, password: &str) -> Result<PdfDocument, PdfError> {
    #[cfg(test)]
    tests::record_parse(path);
    PdfDocument::from_path_with_password(path, password)
}

// ---------------------------------------------------------------------------
// Pinned documents
// ---------------------------------------------------------------------------
//
// Every call reads and parses its file from scratch, which costs memory in
// proportion to the file: roughly twice its size while parsing. A viewer
// makes many calls on one document at once (page renders, page sizes, text
// layout, search), so a large scan multiplied that cost until Android killed
// the app. A caller that will keep using a document pins it with
// `pdf_document_open`; read-only calls on the same path and password then
// share that one parse for as long as the file on disk is unchanged. Files
// that nobody pinned keep the per-call behaviour.

/// What a file looked like when it was parsed. A different length or
/// modification time means a pinned parse no longer describes the file.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn file_stamp(path: &str) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

struct PinnedDocument {
    stamp: FileStamp,
    doc: Arc<PdfDocument>,
    /// Outstanding `pdf_document_open` calls; the parse is freed at zero.
    holders: usize,
}

/// Path and password, exactly as the caller passed them.
type PinKey = (String, String);

fn pinned_documents() -> MutexGuard<'static, HashMap<PinKey, PinnedDocument>> {
    static PINNED: OnceLock<Mutex<HashMap<PinKey, PinnedDocument>>> = OnceLock::new();
    PINNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// A document for a read-only call: the pinned parse when it still matches
/// the file on disk, otherwise a fresh parse that is dropped afterwards.
enum ReadDocument {
    Pinned(Arc<PdfDocument>),
    Fresh(PdfDocument),
}

impl Deref for ReadDocument {
    type Target = PdfDocument;

    fn deref(&self) -> &PdfDocument {
        match self {
            Self::Pinned(doc) => doc,
            Self::Fresh(doc) => doc,
        }
    }
}

fn open_for_reading(path: &str, password: &str) -> Result<ReadDocument, PdfError> {
    if let Some(stamp) = file_stamp(path) {
        let pinned = pinned_documents();
        if let Some(entry) = pinned.get(&(path.to_owned(), password.to_owned())) {
            if entry.stamp == stamp {
                return Ok(ReadDocument::Pinned(Arc::clone(&entry.doc)));
            }
        }
    }
    open(path, password).map(ReadDocument::Fresh)
}

/// Parse a 1-based range string ("1-3,5"; empty = all) into 0-based indices.
fn parse_ranges(spec: &str, page_count: usize) -> Result<Vec<usize>, PdfError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok((0..page_count).collect());
    }
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let lo: usize = lo
            .parse()
            .map_err(|_| PdfError::Structure(format!("bad page range '{part}'")))?;
        let hi: usize = hi
            .parse()
            .map_err(|_| PdfError::Structure(format!("bad page range '{part}'")))?;
        if lo == 0 || hi < lo {
            return Err(PdfError::Structure(format!("bad page range '{part}'")));
        }
        for page in lo..=hi {
            if page > page_count {
                return Err(PdfError::PageIndex(page - 1));
            }
            out.push(page - 1);
        }
    }
    if out.is_empty() {
        return Err(PdfError::Structure("empty page selection".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Exported functions
// ---------------------------------------------------------------------------

/// Library version (static string; do NOT free).
#[no_mangle]
pub extern "C" fn pdf_core_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Borrowed pointer to the last error message on this thread (do NOT free).
#[no_mangle]
pub extern "C" fn pdf_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Borrowed pointer to the warnings from the last render on this thread (do
/// NOT free). Empty when the page rendered exactly as authored.
///
/// Newline-separated. Valid until the next render on the same thread, so a
/// caller reads it immediately after the render call that produced it.
#[no_mangle]
pub extern "C" fn pdf_last_warnings() -> *const c_char {
    LAST_WARNINGS.with(|slot| slot.borrow().as_ptr())
}

/// Free a string returned by this library.
///
/// # Safety
/// `ptr` must be a pointer previously returned by one of the `char*`
/// returning functions of this library (or NULL).
#[no_mangle]
pub unsafe extern "C" fn pdf_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr));
    }
}

/// Pin a document: parse it once, and let read-only calls on the same path
/// and password (page count, inspection, text, text layout, rendering, page
/// size, page extraction, encryption) reuse that parse while the file is
/// unchanged. Returns the page count, or -1 with `pdf_last_error` (for
/// example `ENCRYPTED`) when the document cannot be opened. Every successful
/// call needs a matching `pdf_document_close`.
///
/// # Safety
/// `path` and `password` must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_document_open(
    path: *const c_char,
    password: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return -1;
    };
    run_int(|| {
        let key = (path.to_owned(), password.to_owned());
        let stamp = file_stamp(path);
        if let Some(stamp) = stamp {
            if let Some(entry) = pinned_documents().get_mut(&key) {
                if entry.stamp == stamp {
                    entry.holders += 1;
                    return Ok(entry.doc.page_count().unwrap_or(0) as i64);
                }
            }
        }
        // Parse without holding the lock, so other documents stay usable.
        let doc = Arc::new(open(path, password)?);
        let pages = doc.page_count().unwrap_or(0) as i64;
        let Some(stamp) = stamp else {
            // Readable but not statable: nothing reliable to pin against.
            return Ok(pages);
        };
        let replaced = {
            let mut pinned = pinned_documents();
            match pinned.get_mut(&key) {
                // Another caller pinned the same file meanwhile; share theirs.
                Some(entry) if entry.stamp == stamp => {
                    entry.holders += 1;
                    None
                }
                // The file changed since it was pinned. Every holder moves to
                // the new parse, and the stale one is freed below.
                Some(entry) => {
                    entry.holders += 1;
                    entry.stamp = stamp;
                    Some(std::mem::replace(&mut entry.doc, doc))
                }
                None => {
                    pinned.insert(key, PinnedDocument { stamp, doc, holders: 1 });
                    None
                }
            }
        };
        // Freeing a large document takes a moment; never hold the lock for it.
        drop(replaced);
        Ok(pages)
    })
}

/// Release one `pdf_document_open`. The parse is freed when its last holder
/// closes it; calls already using it finish first. Closing a document that
/// is not pinned does nothing.
///
/// # Safety
/// `path` and `password` must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_document_close(
    path: *const c_char,
    password: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return -1;
    };
    run_int(|| {
        let key = (path.to_owned(), password.to_owned());
        let released = {
            let mut pinned = pinned_documents();
            match pinned.get_mut(&key) {
                Some(entry) if entry.holders > 1 => {
                    entry.holders -= 1;
                    None
                }
                Some(_) => pinned.remove(&key),
                None => None,
            }
        };
        drop(released);
        Ok(0)
    })
}

/// Number of pages, or -1 on error.
///
/// # Safety
/// `path` and `password` must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_page_count(
    path: *const c_char,
    password: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return -1;
    };
    run_int(|| {
        let doc = open_for_reading(path, password)?;
        Ok(doc.page_count().unwrap_or(0) as i64)
    })
}

/// JSON summary: version, encryption, page count, metadata.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_inspect_json(
    path: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open_for_reading(path, password)?;
        let inspect = doc.inspect();
        let metadata = pdf_ops::metadata::read_metadata(&doc);
        let value = serde_json::json!({
            "version": inspect.version,
            "encrypted": inspect.encrypted,
            "objectCount": inspect.object_count,
            "pageCount": inspect.page_count,
            "metadata": metadata,
        });
        Ok(value.to_string())
    })
}

/// Document metadata as JSON.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_get_metadata_json(
    path: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open_for_reading(path, password)?;
        serde_json::to_string(&pdf_ops::metadata::read_metadata(&doc))
            .map_err(|e| PdfError::Structure(e.to_string()))
    })
}

/// Set metadata fields from JSON and save to `out_path`. Unknown JSON keys
/// are ignored; missing keys are left untouched; empty strings delete.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_set_metadata(
    path: *const c_char,
    password: *const c_char,
    metadata_json: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(json), Ok(out)) =
        (cstr(path), cstr(password), cstr(metadata_json), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let mut doc = open(path, password)?;
        let meta: pdf_ops::metadata::DocumentMetadata =
            serde_json::from_str(json).map_err(|e| PdfError::Structure(e.to_string()))?;
        pdf_ops::metadata::write_metadata(&mut doc, &meta)?;
        doc.save_as(out)?;
        Ok(0)
    })
}

/// Copy selected pages (1-based ranges, e.g. "1-3,7") into a new file.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_extract_pages(
    path: *const c_char,
    password: *const c_char,
    pages: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(pages), Ok(out)) =
        (cstr(path), cstr(password), cstr(pages), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let doc = open_for_reading(path, password)?;
        let count = doc.page_count().unwrap_or(0) as usize;
        let indices = parse_ranges(pages, count)?;
        let extracted = pdf_ops::split::extract_pages(&doc, &indices)?;
        extracted.save_as(out)?;
        Ok(0)
    })
}

/// Delete selected pages and save the remainder.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_delete_pages(
    path: *const c_char,
    password: *const c_char,
    pages: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(pages), Ok(out)) =
        (cstr(path), cstr(password), cstr(pages), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let mut doc = open(path, password)?;
        let count = doc.page_count().unwrap_or(0) as usize;
        let indices = parse_ranges(pages, count)?;
        pdf_ops::split::delete_pages(&mut doc, &indices)?;
        doc.save_as(out)?;
        Ok(0)
    })
}

/// Reorder pages. `order` must list every page exactly once (1-based,
/// comma separated, e.g. "3,1,2").
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_reorder_pages(
    path: *const c_char,
    password: *const c_char,
    order: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(order), Ok(out)) =
        (cstr(path), cstr(password), cstr(order), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let mut doc = open(path, password)?;
        let count = doc.page_count().unwrap_or(0) as usize;
        let indices = parse_ranges(order, count)?;
        pdf_ops::split::reorder_pages(&mut doc, &indices)?;
        doc.save_as(out)?;
        Ok(0)
    })
}

/// Merge files. `paths` is newline-separated.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_merge(
    paths: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(paths), Ok(out)) = (cstr(paths), cstr(out_path)) else {
        return -1;
    };
    run_int(|| {
        let list: Vec<&str> = paths
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if list.is_empty() {
            return Err(PdfError::Structure("no input files".into()));
        }
        let merged = pdf_ops::merge::merge_files(&list)?;
        merged.save_as(out)?;
        Ok(0)
    })
}

/// Rotate selected pages (empty selection = all) by `degrees` (multiple
/// of 90, may be negative).
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_rotate_pages(
    path: *const c_char,
    password: *const c_char,
    pages: *const c_char,
    degrees: c_int,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(pages), Ok(out)) =
        (cstr(path), cstr(password), cstr(pages), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let mut doc = open(path, password)?;
        let count = doc.page_count().unwrap_or(0) as usize;
        let indices = parse_ranges(pages, count)?;
        for index in indices {
            pdf_ops::rotate::rotate_page(&mut doc, index, degrees as i64)?;
        }
        doc.save_as(out)?;
        Ok(0)
    })
}

/// Set the crop box of one page (1-based index).
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn pdf_set_crop_box(
    path: *const c_char,
    password: *const c_char,
    page: c_int,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(out)) = (cstr(path), cstr(password), cstr(out_path)) else {
        return -1;
    };
    run_int(|| {
        let mut doc = open(path, password)?;
        if page < 1 {
            return Err(PdfError::Structure("page index is 1-based".into()));
        }
        pdf_ops::rotate::set_crop_box(
            &mut doc,
            (page - 1) as usize,
            pdf_ops::rotate::Rect { x0, y0, x1, y1 },
        )?;
        doc.save_as(out)?;
        Ok(0)
    })
}

/// Extract text. `page` is 1-based; 0 extracts all pages separated by
/// form-feed (\f) characters.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_extract_text(
    path: *const c_char,
    password: *const c_char,
    page: c_int,
) -> *mut c_char {
    let Ok(page) = non_negative_page(page) else {
        return std::ptr::null_mut();
    };
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open_for_reading(path, password)?;
        if page == 0 {
            Ok(pdf_text::extractor::extract_all_pages(&doc)?.join("\u{0C}"))
        } else {
            pdf_text::extractor::extract_page_text(&doc, page - 1)
        }
    })
}

struct RenderTextMetrics(pdf_render::font::RenderFont);

impl pdf_text::layout::LayoutFontMetrics for RenderTextMetrics {
    fn advance_width(&self, code: u32) -> f64 {
        self.0.advance_width(code)
    }

    fn glyph_bounds(&self, code: u32) -> Option<[f64; 4]> {
        let (left, bottom, right, top) = self.0.outline(code)?.bounds()?;
        let scale = 1000.0 / self.0.units_per_em();
        Some([left * scale, bottom * scale, right * scale, top * scale])
    }
}

/// Selectable text for one page (1-based, unlike the rendering API).
/// Returns JSON `{text,width,height,glyphs:[{start,end,bounds:[l,t,r,b]}]}`.
/// Offsets are UTF-16 code units, end exclusive. Bounds use top-left displayed
/// page points, including CropBox origin and /Rotate, matching page rendering.
/// Empty/scanned pages return empty text and glyphs; hidden OCR remains included.
/// Free the returned string with `pdf_free_string`; NULL means `pdf_last_error`.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_page_text_layout_json(
    path: *const c_char,
    password: *const c_char,
    page: c_int,
) -> *mut c_char {
    if page < 1 {
        set_error(format!("PAGE_OUT_OF_RANGE: page index {page} must be 1-based"));
        return std::ptr::null_mut();
    }
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open_for_reading(path, password)?;
        let layout = pdf_text::layout::extract_page_layout_with_metrics(
            &doc,
            (page - 1) as usize,
            &|doc, dict| Box::new(RenderTextMetrics(pdf_render::font::RenderFont::load(doc, dict))),
        )?;
        serde_json::to_string(&layout).map_err(|e| PdfError::Structure(e.to_string()))
    })
}

/// AI-ready export. `ndjson` 0 = single JSON document, 1 = NDJSON lines.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_export_ai(
    path: *const c_char,
    password: *const c_char,
    max_chars: c_int,
    overlap: c_int,
    ndjson: c_int,
) -> *mut c_char {
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open_for_reading(path, password)?;
        let mut options = ChunkOptions::default();
        if max_chars > 0 {
            options.max_chars = max_chars as usize;
        }
        if overlap >= 0 {
            options.overlap = overlap as usize;
        }
        if ndjson != 0 {
            pdf_ai::export::to_ndjson(&doc, options)
        } else {
            pdf_ai::export::to_json(&doc, options)
        }
    })
}

/// Encrypt with AES-256 (PDF 2.0). Empty owner password reuses the user's.
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_encrypt(
    path: *const c_char,
    password: *const c_char,
    user_password: *const c_char,
    owner_password: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(user_pw), Ok(owner_pw), Ok(out)) = (
        cstr(path),
        cstr(password),
        cstr(user_password),
        cstr(owner_password),
        cstr(out_path),
    ) else {
        return -1;
    };
    run_int(|| {
        let doc = open_for_reading(path, password)?;
        let bytes = encrypt_to_bytes(&doc, user_pw, owner_pw)?;
        pdf_core::writer::PdfWriter::write_bytes_atomic(&bytes, out)?;
        Ok(0)
    })
}

/// Remove encryption (requires the correct password).
///
/// # Safety
/// See `pdf_page_count`.
#[no_mangle]
pub unsafe extern "C" fn pdf_decrypt(
    path: *const c_char,
    password: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(out)) = (cstr(path), cstr(password), cstr(out_path)) else {
        return -1;
    };
    run_int(|| {
        let doc = open_for_reading(path, password)?;
        doc.save_as(out)?; // the standard writer always writes decrypted
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// Milestone 13: rendering and image composition
// ---------------------------------------------------------------------------

/// Release a buffer handed out by `pdf_render_page_*`.
///
/// # Safety
/// `ptr`/`len` must be exactly what the render call returned, and must not
/// have been freed already.
#[no_mangle]
pub unsafe extern "C" fn pdf_free_buffer(ptr: *mut u8, len: i32) {
    if ptr.is_null() || len <= 0 {
        return;
    }
    drop(Vec::from_raw_parts(ptr, len as usize, len as usize));
}

/// Hand a `Vec<u8>` to the caller as a raw pointer, writing its length into
/// `out_len`. Returns NULL on failure.
fn release_buffer(data: Vec<u8>, out_len: *mut i32) -> *mut u8 {
    if out_len.is_null() {
        set_error("ERROR: null out_len");
        return std::ptr::null_mut();
    }
    let mut boxed = data.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    let len = boxed.len();
    if len > i32::MAX as usize {
        set_error("ERROR: rendered buffer too large");
        return std::ptr::null_mut();
    }
    std::mem::forget(boxed);
    unsafe { *out_len = len as i32 };
    ptr
}

fn render_options(target_width: i32, target_height: i32) -> pdf_render::RenderOptions {
    let size = if target_width > 0 && target_height > 0 {
        pdf_render::RenderSize::FitBox {
            width: target_width as u32,
            height: target_height as u32,
        }
    } else {
        // Negative/zero target means "use the page's own size at 72 dpi".
        pdf_render::RenderSize::Scale(1.0)
    };
    pdf_render::RenderOptions {
        size,
        ..Default::default()
    }
}

/// Render page `page` (0-based) and return it as PNG bytes.
///
/// Fits inside `target_width` x `target_height` pixels when both are positive.
/// Free the result with `pdf_free_buffer`. On failure, returns NULL and sets
/// `out_len` to zero. Negative page indices are errors.
///
/// # Safety
/// All pointer arguments must be valid NUL-terminated C strings / writable
/// out-parameters for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn pdf_render_page_png(
    path: *const c_char,
    password: *const c_char,
    page: i32,
    target_width: i32,
    target_height: i32,
    out_len: *mut i32,
) -> *mut u8 {
    set_warnings(&[]);
    if out_len.is_null() {
        set_error("ERROR: null out_len");
        return std::ptr::null_mut();
    }
    *out_len = 0;
    let Ok(page) = non_negative_page(page) else {
        return std::ptr::null_mut();
    };
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Vec<u8>, PdfError> {
        let doc = open_for_reading(path, password)?;
        let rendered =
            pdf_render::render_page(&doc, page, render_options(target_width, target_height))?;
        set_warnings(&rendered.warnings);
        pdf_render::encode_rgba_as_png(&rendered.pixels, rendered.width, rendered.height)
            .ok_or_else(|| PdfError::Structure("could not encode PNG".into()))
    }));
    match result {
        Ok(Ok(data)) => release_buffer(data, out_len),
        Ok(Err(err)) => {
            set_pdf_error(&err);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error("PANIC: internal error");
            std::ptr::null_mut()
        }
    }
}

/// Render page `page` (0-based) as raw RGBA8, top-left origin.
///
/// Writes the pixel dimensions into `out_width`/`out_height` so the caller can
/// hand the buffer straight to a GPU upload without re-parsing a container.
/// Free the result with `pdf_free_buffer`. All non-null output parameters are
/// set to zero on failure. Negative page indices are errors.
///
/// # Safety
/// As [`pdf_render_page_png`].
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn pdf_render_page_rgba(
    path: *const c_char,
    password: *const c_char,
    page: i32,
    target_width: i32,
    target_height: i32,
    out_width: *mut i32,
    out_height: *mut i32,
    out_len: *mut i32,
) -> *mut u8 {
    set_warnings(&[]);
    for output in [out_width, out_height, out_len] {
        if !output.is_null() {
            *output = 0;
        }
    }
    if out_width.is_null() || out_height.is_null() || out_len.is_null() {
        set_error("ERROR: null out parameter");
        return std::ptr::null_mut();
    }
    let Ok(page) = non_negative_page(page) else {
        return std::ptr::null_mut();
    };
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<_, PdfError> {
        let doc = open_for_reading(path, password)?;
        pdf_render::render_page(&doc, page, render_options(target_width, target_height))
    }));
    match result {
        Ok(Ok(rendered)) => {
            let buffer = release_buffer(rendered.pixels, out_len);
            if !buffer.is_null() {
                *out_width = rendered.width as i32;
                *out_height = rendered.height as i32;
                set_warnings(&rendered.warnings);
            }
            buffer
        }
        Ok(Err(err)) => {
            set_pdf_error(&err);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error("PANIC: internal error");
            std::ptr::null_mut()
        }
    }
}

/// Page size in PostScript points, honouring `/Rotate`. `page` is 0-based.
/// Non-null output parameters are set to zero on failure.
///
/// # Safety
/// `out_width`/`out_height` must be writable.
#[no_mangle]
pub unsafe extern "C" fn pdf_page_size(
    path: *const c_char,
    password: *const c_char,
    page: i32,
    out_width: *mut f64,
    out_height: *mut f64,
) -> c_int {
    for output in [out_width, out_height] {
        if !output.is_null() {
            *output = 0.0;
        }
    }
    if out_width.is_null() || out_height.is_null() {
        set_error("ERROR: null out parameter");
        return -1;
    }
    let Ok(page) = non_negative_page(page) else {
        return -1;
    };
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return -1;
    };
    let (w, h) = match catch_unwind(AssertUnwindSafe(|| -> Result<(f64, f64), PdfError> {
        let doc = open_for_reading(path, password)?;
        pdf_render::page_size_points(&doc, page)
    })) {
        Ok(Ok(size)) => size,
        Ok(Err(err)) => {
            set_pdf_error(&err);
            return -1;
        }
        Err(_) => {
            set_error("PANIC: internal error");
            return -1;
        }
    };
    *out_width = w;
    *out_height = h;
    0
}

/// Build a PDF from JPEG images, one page each.
///
/// `jpeg_paths` is a newline-separated list, in page order. `fit` is 0 for
/// "fixed page size, image letterboxed" and 1 for "page takes the image's
/// aspect ratio". `page_width`/`page_height` are in points; pass 0 for A4.
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_images_to_pdf(
    jpeg_paths: *const c_char,
    out_path: *const c_char,
    page_width: f64,
    page_height: f64,
    fit: i32,
    margin: f64,
) -> c_int {
    let (Ok(paths), Ok(out_path)) = (cstr(jpeg_paths), cstr(out_path)) else {
        return -1;
    };
    run_int(|| {
        let files: Vec<&str> = paths
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if files.is_empty() {
            return Err(PdfError::Structure("no input images".into()));
        }
        let mut images = Vec::with_capacity(files.len());
        for file in files {
            images.push(std::fs::read(file)?);
        }
        let options = ComposeOptions {
            page_size: if page_width > 1.0 && page_height > 1.0 {
                (page_width, page_height)
            } else {
                pdf_ops::compose::A4
            },
            fit: if fit == 0 {
                PageFit::Contain
            } else {
                PageFit::ImageAspect
            },
            margin,
        };
        let doc = images_to_document(&images, options)?;
        doc.save_as(out_path)?;
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// Milestone 14: OCR
// ---------------------------------------------------------------------------

/// OCR options as JSON; every field is optional and an empty string means
/// all defaults: `{"dpi":300,"force":false,"minConfidence":0.5,"deskew":true,
/// "detectOrientation":true,"threads":0}`.
#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct OcrOptionsJson {
    dpi: Option<f64>,
    force: bool,
    min_confidence: Option<f32>,
    deskew: Option<bool>,
    detect_orientation: Option<bool>,
    threads: Option<usize>,
}

fn ocr_options(json: &str) -> Result<pdf_ocr::PdfOcrOptions, PdfError> {
    let parsed: OcrOptionsJson = if json.trim().is_empty() {
        OcrOptionsJson::default()
    } else {
        serde_json::from_str(json).map_err(|e| PdfError::Structure(format!("OCR options: {e}")))?
    };
    let mut options = pdf_ocr::PdfOcrOptions::default();
    if let Some(dpi) = parsed.dpi {
        options.dpi = dpi;
    }
    options.force = parsed.force;
    if let Some(confidence) = parsed.min_confidence {
        options.engine.min_confidence = confidence.clamp(0.0, 1.0);
    }
    if let Some(deskew) = parsed.deskew {
        options.engine.deskew = deskew;
    }
    if let Some(detect) = parsed.detect_orientation {
        options.engine.detect_orientation = detect;
    }
    if let Some(threads) = parsed.threads {
        options.engine.threads = threads.min(64);
    }
    Ok(options)
}

/// Recognize the text on one page (1-based) with the built-in engine.
///
/// Returns JSON `{page,status,width,height,dpi,skewDegrees,orientationDegrees,confidence,text,
/// lines:[{text,confidence,bounds,words:[{text,confidence,quad,bounds}]}],
/// layout}`. Coordinates are displayed page points with a top-left origin,
/// like `pdf_page_text_layout_json`, and `layout` is exactly what that
/// function will report once the page carries this text (see
/// `pdf_make_searchable_json`). `status` is `recognized` or `noText`; pages
/// that already have text report `hasText` or `hasOcrLayer` with no lines
/// and a null layout, unless `force` is set. Options: see `OcrOptionsJson`.
/// Free the result with `pdf_free_string`; NULL means `pdf_last_error`.
///
/// # Safety
/// All pointers must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_ocr_page_json(
    path: *const c_char,
    password: *const c_char,
    page: c_int,
    options_json: *const c_char,
) -> *mut c_char {
    if page < 1 {
        set_error(format!(
            "PAGE_OUT_OF_RANGE: page index {page} must be 1-based"
        ));
        return std::ptr::null_mut();
    }
    let (Ok(path), Ok(password), Ok(options)) = (cstr(path), cstr(password), cstr(options_json))
    else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let options = ocr_options(options)?;
        let doc = open_for_reading(path, password)?;
        let index = (page - 1) as usize;
        if index >= doc.page_count().unwrap_or(0) as usize {
            return Err(PdfError::PageIndex(index));
        }
        if !options.force {
            if let Some(status) = pdf_ocr::pdf::existing_text(&doc, index)? {
                let value = serde_json::json!({
                    "page": page, "status": status, "text": "", "confidence": 0.0,
                    "lines": [], "layout": null,
                });
                return Ok(value.to_string());
            }
        }
        let ocr =
            pdf_ocr::pdf::recognize_page(&doc, index, pdf_ocr::OcrEngine::embedded(), &options)?;
        let layout = pdf_ocr::pdf::ocr_layout(&doc, &ocr)?;
        let status = if ocr.lines.is_empty() {
            pdf_ocr::PageStatus::NoText
        } else {
            pdf_ocr::PageStatus::Recognized
        };
        let value = serde_json::json!({
            "page": page, "status": status, "width": ocr.width, "height": ocr.height,
            "dpi": ocr.dpi, "skewDegrees": ocr.skew_degrees,
            "orientationDegrees": ocr.orientation_degrees, "confidence": ocr.mean_confidence(),
            "text": ocr.text(), "lines": ocr.lines, "layout": layout,
        });
        Ok(value.to_string())
    })
}

/// Recognize the selected pages ("1-3,5"; empty = all) and save a copy with
/// invisible text layers to `out_path`, so their text can be searched,
/// selected and extracted. Pages that already have text are left alone
/// unless `force` is set. Like the other writing operations, the copy is
/// saved without encryption.
///
/// Returns a JSON report `{recognized, pages:[{page,status,words,confidence}]}`.
/// Free it with `pdf_free_string`; NULL means `pdf_last_error`.
///
/// # Safety
/// All pointers must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_make_searchable_json(
    path: *const c_char,
    password: *const c_char,
    pages: *const c_char,
    options_json: *const c_char,
    out_path: *const c_char,
) -> *mut c_char {
    let (Ok(path), Ok(password), Ok(pages), Ok(options), Ok(out)) = (
        cstr(path),
        cstr(password),
        cstr(pages),
        cstr(options_json),
        cstr(out_path),
    ) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let options = ocr_options(options)?;
        let mut doc = open(path, password)?;
        let indices = parse_ranges(pages, doc.page_count().unwrap_or(0) as usize)?;
        let reports = pdf_ocr::pdf::make_searchable(
            &mut doc,
            &indices,
            pdf_ocr::OcrEngine::embedded(),
            &options,
        )?;
        doc.save_as(out)?;
        let recognized = reports
            .iter()
            .filter(|r| r.status == pdf_ocr::PageStatus::Recognized)
            .count();
        Ok(serde_json::json!({ "recognized": recognized, "pages": reports }).to_string())
    })
}

/// Save a copy of the document with invisible text layers built from OCR
/// results computed elsewhere: by another engine, or by `pdf_ocr_page_json`
/// on each page in turn (which lets a caller show progress page by page and
/// still write the file once).
///
/// `ocr_json` is an array of `{page, lines:[{words:[{text, quad|bounds}]}]}`
/// in displayed page points; pages are 1-based. Returns the number of pages
/// that received text, or -1 with `pdf_last_error`.
///
/// # Safety
/// All pointers must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pdf_apply_ocr_json(
    path: *const c_char,
    password: *const c_char,
    ocr_json: *const c_char,
    out_path: *const c_char,
) -> c_int {
    let (Ok(path), Ok(password), Ok(json), Ok(out)) =
        (cstr(path), cstr(password), cstr(ocr_json), cstr(out_path))
    else {
        return -1;
    };
    run_int(|| {
        let pages: Vec<pdf_ocr::pdf::ExternalPage> = serde_json::from_str(json)
            .map_err(|e| PdfError::Structure(format!("OCR results: {e}")))?;
        let mut doc = open(path, password)?;
        let written = pdf_ocr::pdf::apply_ocr(&mut doc, &pages)?;
        doc.save_as(out)?;
        Ok(written as i64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn fixture_path() -> CString {
        c(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/simple.pdf"
        ))
    }

    /// Full parses per path, so tests can tell a pinned document from a
    /// fresh read. Keyed by path because tests run in parallel.
    fn parse_counts() -> MutexGuard<'static, HashMap<String, usize>> {
        static PARSES: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
        PARSES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn record_parse(path: &str) {
        *parse_counts().entry(path.to_owned()).or_default() += 1;
    }

    fn parses(path: &str) -> usize {
        parse_counts().get(path).copied().unwrap_or(0)
    }

    /// A fresh directory per test, so parallel tests never share files.
    fn scratch_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pdf_ffi_{}_{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pinned_documents_share_one_parse_until_the_last_close() {
        let dir = scratch_dir("pin_share");
        let path = dir.join("doc.pdf");
        std::fs::write(&path, include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let path_str = path.to_str().unwrap();
        let (p, empty) = (c(path_str), c(""));
        unsafe {
            assert_eq!(pdf_document_open(p.as_ptr(), empty.as_ptr()), 1);
            assert_eq!(parses(path_str), 1);

            // Every read-only call reuses the pinned parse.
            assert_eq!(pdf_page_count(p.as_ptr(), empty.as_ptr()), 1);
            let mut len = 0;
            let png = pdf_render_page_png(p.as_ptr(), empty.as_ptr(), 0, 64, 64, &mut len);
            assert!(!png.is_null());
            pdf_free_buffer(png, len);
            let (mut w, mut h) = (0.0, 0.0);
            assert_eq!(pdf_page_size(p.as_ptr(), empty.as_ptr(), 0, &mut w, &mut h), 0);
            let layout = pdf_page_text_layout_json(p.as_ptr(), empty.as_ptr(), 1);
            assert!(!layout.is_null());
            pdf_free_string(layout);
            let text = pdf_extract_text(p.as_ptr(), empty.as_ptr(), 0);
            assert!(!text.is_null());
            pdf_free_string(text);
            assert_eq!(parses(path_str), 1);

            // A second holder shares the same parse.
            assert_eq!(pdf_document_open(p.as_ptr(), empty.as_ptr()), 1);
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
            assert_eq!(pdf_page_count(p.as_ptr(), empty.as_ptr()), 1);
            assert_eq!(parses(path_str), 1);

            // After the last close, calls read the file again.
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
            assert_eq!(pdf_page_count(p.as_ptr(), empty.as_ptr()), 1);
            assert_eq!(parses(path_str), 2);

            // Closing an unpinned document is harmless.
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_file_is_read_afresh_instead_of_its_stale_pin() {
        let dir = scratch_dir("pin_stale");
        let path = dir.join("doc.pdf");
        std::fs::write(&path, include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let (p, empty) = (c(path.to_str().unwrap()), c(""));
        unsafe {
            assert_eq!(pdf_document_open(p.as_ptr(), empty.as_ptr()), 1);
            std::fs::write(&path, include_bytes!("../../../fixtures/two_pages.pdf")).unwrap();
            assert_eq!(pdf_page_count(p.as_ptr(), empty.as_ptr()), 2);

            // Pinning again replaces the stale parse for both holders.
            assert_eq!(pdf_document_open(p.as_ptr(), empty.as_ptr()), 2);
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
            assert_eq!(pdf_page_count(p.as_ptr(), empty.as_ptr()), 2);
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_open_pins_nothing_and_keeps_the_error_code() {
        let dir = scratch_dir("pin_locked");
        let plain = dir.join("plain.pdf");
        std::fs::write(&plain, include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let locked = dir.join("locked.pdf");
        let locked_str = locked.to_str().unwrap();
        let (plain_c, locked_c, empty, pw) =
            (c(plain.to_str().unwrap()), c(locked_str), c(""), c("secret"));
        unsafe {
            assert_eq!(
                pdf_encrypt(plain_c.as_ptr(), empty.as_ptr(), pw.as_ptr(), empty.as_ptr(), locked_c.as_ptr()),
                0
            );
            assert_eq!(pdf_document_open(locked_c.as_ptr(), empty.as_ptr()), -1);
            assert!(last_error().starts_with("ENCRYPTED"), "got: {}", last_error());
            let wrong = c("wrong");
            assert_eq!(pdf_document_open(locked_c.as_ptr(), wrong.as_ptr()), -1);
            assert!(last_error().starts_with("WRONG_PASSWORD"), "got: {}", last_error());

            let before = parses(locked_str);
            assert_eq!(pdf_page_count(locked_c.as_ptr(), empty.as_ptr()), -1);
            assert_eq!(parses(locked_str), before + 1, "nothing was pinned");

            assert_eq!(pdf_document_open(locked_c.as_ptr(), pw.as_ptr()), 1);
            let pinned_at = parses(locked_str);
            assert_eq!(pdf_page_count(locked_c.as_ptr(), pw.as_ptr()), 1);
            assert_eq!(parses(locked_str), pinned_at);
            // The pin is per password: other passwords still read the file.
            assert_eq!(pdf_page_count(locked_c.as_ptr(), empty.as_ptr()), -1);
            assert_eq!(pdf_document_close(locked_c.as_ptr(), pw.as_ptr()), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_calls_share_a_pinned_document_safely() {
        let dir = scratch_dir("pin_threads");
        let path = dir.join("doc.pdf");
        std::fs::write(&path, include_bytes!("../../../fixtures/two_pages.pdf")).unwrap();
        let path_str = path.to_str().unwrap().to_owned();
        let (p, empty) = (c(&path_str), c(""));
        unsafe {
            assert_eq!(pdf_document_open(p.as_ptr(), empty.as_ptr()), 2);
        }
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let path = path_str.clone();
                std::thread::spawn(move || {
                    let (p, empty) = (c(&path), c(""));
                    for round in 0..5 {
                        let page = (worker + round) % 2;
                        unsafe {
                            let mut len = 0;
                            let png = pdf_render_page_png(p.as_ptr(), empty.as_ptr(), page, 48, 48, &mut len);
                            assert!(!png.is_null(), "render failed: {}", last_error());
                            pdf_free_buffer(png, len);
                            let layout = pdf_page_text_layout_json(p.as_ptr(), empty.as_ptr(), page + 1);
                            assert!(!layout.is_null(), "layout failed: {}", last_error());
                            pdf_free_string(layout);
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(parses(&path_str), 1);
        unsafe {
            assert_eq!(pdf_document_close(p.as_ptr(), empty.as_ptr()), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn selectable_bounds_cover_rendered_text_with_substituted_fonts_and_transforms() {
        use pdf_core::object::{Dictionary, PdfObject};
        use pdf_core::stream::PdfStream;
        use pdf_render::page::{render_page, RenderOptions};

        for rotation in [0, 90, 180, 270] {
            for transform in ["", "0.9 0.15 0.2 1 0 0 cm"] {
                let mut doc = PdfDocument::from_bytes(include_bytes!("../../../fixtures/simple.pdf")).unwrap();
                let font = Dictionary::from([
                    ("Type".into(), PdfObject::Name("Font".into())),
                    ("Subtype".into(), PdfObject::Name("Type1".into())),
                    ("BaseFont".into(), PdfObject::Name("Helvetica-Oblique".into())),
                ]);
                let font_id = doc.add_object(PdfObject::Dictionary(font));
                let content = format!("q {transform} BT /F1 10 Tf 30 70 Td (WiWi thin and wide) Tj ET Q");
                let stream_id = doc.add_object(PdfObject::Stream(PdfStream::new(Dictionary::new(), content.into_bytes())));
                let page_id = doc.collect_page_ids().unwrap()[0];
                let mut page = doc.resolve(page_id).unwrap().as_dict().unwrap().clone();
                page.insert("Resources".into(), PdfObject::Dictionary(Dictionary::from([
                    ("Font".into(), PdfObject::Dictionary(Dictionary::from([
                        ("F1".into(), PdfObject::Reference(font_id)),
                    ]))),
                ])));
                page.insert("Contents".into(), PdfObject::Reference(stream_id));
                page.insert("Rotate".into(), PdfObject::Integer(rotation));
                page.insert("CropBox".into(), PdfObject::Array([10, 20, 190, 180].into_iter().map(PdfObject::Integer).collect()));
                doc.set_object(page_id, PdfObject::Dictionary(page));
                let layout = pdf_text::layout::extract_page_layout_with_metrics(&doc, 0, &|doc, dict| {
                    Box::new(RenderTextMetrics(pdf_render::font::RenderFont::load(doc, dict)))
                }).unwrap();
                let rendered = render_page(&doc, 0, RenderOptions::default()).unwrap();
                assert_eq!((layout.width, layout.height), (rendered.width as f64, rendered.height as f64));
                assert_eq!(layout.text, "WiWi thin and wide");
                let mut ink_pixels = 0;
                for (index, pixel) in rendered.pixels.chunks_exact(4).enumerate() {
                    if pixel[0] < 240 {
                        ink_pixels += 1;
                        let x = (index % rendered.width as usize) as f64 + 0.5;
                        let y = (index / rendered.width as usize) as f64 + 0.5;
                        assert!(layout.glyphs.iter().any(|glyph| {
                            let [l, t, r, b] = glyph.bounds;
                            x >= l - 1.0 && x <= r + 1.0 && y >= t - 1.0 && y <= b + 1.0
                        }), "uncovered pixel ({x},{y}), rotate {rotation}, transform {transform}");
                    }
                }
                assert!(ink_pixels > 50);
            }
        }
    }

    #[test]
    fn text_layout_ffi_reports_blank_size_and_rejects_nonpositive_pages() {
        let path = fixture_path();
        let empty = c("");
        unsafe {
            for page in [-1, 0, 2] {
                let result = pdf_page_text_layout_json(path.as_ptr(), empty.as_ptr(), page);
                assert!(result.is_null());
                assert!(last_error().starts_with("PAGE_OUT_OF_RANGE:"));
            }
            let result = pdf_page_text_layout_json(path.as_ptr(), empty.as_ptr(), 1);
            assert!(!result.is_null());
            let parsed: serde_json::Value = serde_json::from_str(CStr::from_ptr(result).to_str().unwrap()).unwrap();
            pdf_free_string(result);
            assert_eq!(parsed["text"], "");
            assert_eq!(parsed["glyphs"], serde_json::json!([]));
            assert_eq!(parsed["width"], 200.);
            assert_eq!(parsed["height"], 200.);
        }
    }

    #[test]
    fn negative_page_indices_are_errors() {
        let path = fixture_path();
        let empty = c("");
        unsafe {
            for page in [-1, i32::MIN] {
                let mut len = 0;
                let png =
                    pdf_render_page_png(path.as_ptr(), empty.as_ptr(), page, 10, 10, &mut len);
                pdf_free_buffer(png, len);
                assert!(png.is_null());
                assert!(CStr::from_ptr(pdf_last_error())
                    .to_str()
                    .unwrap()
                    .starts_with("PAGE_OUT_OF_RANGE:"));
                let (mut width, mut height) = (0, 0);
                let rgba = pdf_render_page_rgba(
                    path.as_ptr(),
                    empty.as_ptr(),
                    page,
                    10,
                    10,
                    &mut width,
                    &mut height,
                    &mut len,
                );
                pdf_free_buffer(rgba, len);
                assert!(rgba.is_null());
                assert!(CStr::from_ptr(pdf_last_error())
                    .to_str()
                    .unwrap()
                    .starts_with("PAGE_OUT_OF_RANGE:"));
                let (mut w, mut h) = (0.0, 0.0);
                assert_eq!(
                    pdf_page_size(path.as_ptr(), empty.as_ptr(), page, &mut w, &mut h),
                    -1
                );
                assert!(CStr::from_ptr(pdf_last_error())
                    .to_str()
                    .unwrap()
                    .starts_with("PAGE_OUT_OF_RANGE:"));
            }
        }
    }

    #[test]
    fn text_extraction_rejects_negative_pages_but_zero_still_means_all() {
        let path = fixture_path();
        let empty = c("");
        unsafe {
            let invalid = pdf_extract_text(path.as_ptr(), empty.as_ptr(), -1);
            pdf_free_string(invalid);
            assert!(invalid.is_null());
            assert!(CStr::from_ptr(pdf_last_error())
                .to_str()
                .unwrap()
                .starts_with("PAGE_OUT_OF_RANGE:"));
            let all = pdf_extract_text(path.as_ptr(), empty.as_ptr(), 0);
            let first = pdf_extract_text(path.as_ptr(), empty.as_ptr(), 1);
            assert!(!all.is_null() && !first.is_null());
            assert_eq!(CStr::from_ptr(all), CStr::from_ptr(first));
            pdf_free_string(all);
            pdf_free_string(first);
        }
    }

    #[test]
    fn failed_renders_clear_outputs_and_previous_warnings() {
        let path = fixture_path();
        let empty = c("");
        unsafe {
            for input in [path.as_ptr(), std::ptr::null()] {
                let mut len = 123;
                set_warnings(&["previous render".to_owned()]);
                assert!(pdf_render_page_png(input, empty.as_ptr(), 99, 10, 10, &mut len).is_null());
                assert_eq!(len, 0);
                assert!(CStr::from_ptr(pdf_last_warnings()).to_bytes().is_empty());
                let (mut width, mut height, mut len) = (123, 123, 123);
                set_warnings(&["previous render".to_owned()]);
                assert!(pdf_render_page_rgba(
                    input,
                    empty.as_ptr(),
                    99,
                    10,
                    10,
                    &mut width,
                    &mut height,
                    &mut len
                )
                .is_null());
                assert_eq!((width, height, len), (0, 0, 0));
                assert!(CStr::from_ptr(pdf_last_warnings()).to_bytes().is_empty());
                let (mut w, mut h) = (123.0, 123.0);
                assert_eq!(pdf_page_size(input, empty.as_ptr(), 99, &mut w, &mut h), -1);
                assert_eq!((w, h), (0.0, 0.0));
            }
        }
    }

    #[test]
    fn null_render_outputs_fail_before_rendering() {
        let path = fixture_path();
        let empty = c("");
        unsafe {
            assert!(pdf_render_page_png(
                path.as_ptr(),
                empty.as_ptr(),
                0,
                10,
                10,
                std::ptr::null_mut()
            )
            .is_null());
            let (mut width, mut height) = (123, 123);
            assert!(pdf_render_page_rgba(
                path.as_ptr(),
                empty.as_ptr(),
                0,
                10,
                10,
                &mut width,
                &mut height,
                std::ptr::null_mut()
            )
            .is_null());
            assert_eq!((width, height), (0, 0));
        }
    }

    #[test]
    fn ocr_reads_a_scan_and_its_text_then_comes_out_of_the_text_apis() {
        let dir = scratch_dir("ocr");
        let scan = dir.join("scan.pdf");
        std::fs::write(&scan, include_bytes!("../../../fixtures/scanned.pdf")).unwrap();
        let (scan_c, empty) = (c(scan.to_str().unwrap()), c(""));
        let json_at = |ptr: *mut c_char| -> serde_json::Value {
            assert!(!ptr.is_null(), "{}", unsafe { last_error() });
            let value =
                serde_json::from_str(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap()).unwrap();
            unsafe { pdf_free_string(ptr) };
            value
        };
        unsafe {
            let page = json_at(pdf_ocr_page_json(
                scan_c.as_ptr(),
                empty.as_ptr(),
                1,
                empty.as_ptr(),
            ));
            assert_eq!(page["status"], "recognized");
            let text = page["text"].as_str().unwrap();
            assert!(
                text.contains("invoice") && text.contains("1,234.56"),
                "{text}"
            );

            // Writing what was read gives exactly the layout promised.
            let applied = dir.join("applied.pdf");
            let applied_c = c(applied.to_str().unwrap());
            let results =
                c(&serde_json::json!([{ "page": 1, "lines": page["lines"] }]).to_string());
            assert_eq!(
                pdf_apply_ocr_json(
                    scan_c.as_ptr(),
                    empty.as_ptr(),
                    results.as_ptr(),
                    applied_c.as_ptr()
                ),
                1
            );
            let layout = json_at(pdf_page_text_layout_json(
                applied_c.as_ptr(),
                empty.as_ptr(),
                1,
            ));
            assert_eq!(layout, page["layout"]);

            // Or in one step; a second pass then leaves the page alone.
            let searchable = c(dir.join("searchable.pdf").to_str().unwrap());
            let report = json_at(pdf_make_searchable_json(
                scan_c.as_ptr(),
                empty.as_ptr(),
                empty.as_ptr(),
                empty.as_ptr(),
                searchable.as_ptr(),
            ));
            assert_eq!(report["recognized"], 1);
            assert_eq!(report["pages"][0]["status"], "recognized");
            let extracted = pdf_extract_text(searchable.as_ptr(), empty.as_ptr(), 1);
            assert!(CStr::from_ptr(extracted)
                .to_str()
                .unwrap()
                .contains("invoice"));
            pdf_free_string(extracted);
            let again = json_at(pdf_ocr_page_json(
                searchable.as_ptr(),
                empty.as_ptr(),
                1,
                empty.as_ptr(),
            ));
            assert_eq!(
                (again["status"].as_str(), again["layout"].is_null()),
                (Some("hasOcrLayer"), true)
            );

            for (page, options, code) in [
                (2, "", "PAGE_OUT_OF_RANGE"),
                (0, "", "PAGE_OUT_OF_RANGE"),
                (1, r#"{"dpi": 5}"#, "ERROR"),
                (1, r#"{"colour": true}"#, "ERROR"),
            ] {
                let options = c(options);
                assert!(
                    pdf_ocr_page_json(scan_c.as_ptr(), empty.as_ptr(), page, options.as_ptr())
                        .is_null()
                );
                assert!(
                    last_error().starts_with(code),
                    "{page} {options:?}: {}",
                    last_error()
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_ranges("", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_ranges("1-2,4", 5).unwrap(), vec![0, 1, 3]);
        assert_eq!(parse_ranges("3", 3).unwrap(), vec![2]);
        assert!(parse_ranges("0", 3).is_err());
        assert!(parse_ranges("4", 3).is_err());
        assert!(parse_ranges("2-1", 3).is_err());
        assert!(parse_ranges("x", 3).is_err());
    }

    #[test]
    fn ffi_round_trip_on_fixture() {
        // Write the fixture to a temp file, then exercise the C ABI.
        let dir = std::env::temp_dir().join("pdf_ffi_test");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("simple.pdf");
        std::fs::write(&input, include_bytes!("../../../fixtures/simple.pdf")).unwrap();
        let input_c = c(input.to_str().unwrap());
        let empty = c("");

        unsafe {
            assert_eq!(pdf_page_count(input_c.as_ptr(), empty.as_ptr()), 1);

            let json = pdf_inspect_json(input_c.as_ptr(), empty.as_ptr());
            assert!(!json.is_null());
            let parsed: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(json).to_str().unwrap()).unwrap();
            assert_eq!(parsed["pageCount"], 1);
            pdf_free_string(json);

            // Encrypt, then verify password gating + decrypt.
            let enc_path = dir.join("enc.pdf");
            let enc_c = c(enc_path.to_str().unwrap());
            let pw = c("secret");
            assert_eq!(
                pdf_encrypt(
                    input_c.as_ptr(),
                    empty.as_ptr(),
                    pw.as_ptr(),
                    empty.as_ptr(),
                    enc_c.as_ptr()
                ),
                0
            );
            assert_eq!(pdf_page_count(enc_c.as_ptr(), empty.as_ptr()), -1);
            let err = CStr::from_ptr(pdf_last_error()).to_str().unwrap();
            assert!(err.starts_with("ENCRYPTED"), "got: {err}");
            assert_eq!(pdf_page_count(enc_c.as_ptr(), pw.as_ptr()), 1);

            let dec_path = dir.join("dec.pdf");
            let dec_c = c(dec_path.to_str().unwrap());
            assert_eq!(pdf_decrypt(enc_c.as_ptr(), pw.as_ptr(), dec_c.as_ptr()), 0);
            assert_eq!(pdf_page_count(dec_c.as_ptr(), empty.as_ptr()), 1);

            // Merge the original with itself and extract page 2.
            let merged_path = dir.join("merged.pdf");
            let merged_c = c(merged_path.to_str().unwrap());
            let inputs = c(&format!(
                "{}\n{}",
                input.to_str().unwrap(),
                input.to_str().unwrap()
            ));
            assert_eq!(pdf_merge(inputs.as_ptr(), merged_c.as_ptr()), 0);
            assert_eq!(pdf_page_count(merged_c.as_ptr(), empty.as_ptr()), 2);

            let split_path = dir.join("split.pdf");
            let split_c = c(split_path.to_str().unwrap());
            let range = c("2");
            assert_eq!(
                pdf_extract_pages(
                    merged_c.as_ptr(),
                    empty.as_ptr(),
                    range.as_ptr(),
                    split_c.as_ptr()
                ),
                0
            );
            assert_eq!(pdf_page_count(split_c.as_ptr(), empty.as_ptr()), 1);

            // --- Milestone 13: rendering -------------------------------
            let mut out_len = 0i32;
            let png = pdf_render_page_png(
                input_c.as_ptr(),
                empty.as_ptr(),
                0,
                120,
                120,
                &mut out_len,
            );
            assert!(!png.is_null(), "render failed: {}", last_error());
            assert!(out_len > 8);
            let header = std::slice::from_raw_parts(png, 8);
            assert_eq!(header, &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
            pdf_free_buffer(png, out_len);

            let (mut w, mut h, mut len) = (0i32, 0i32, 0i32);
            let rgba = pdf_render_page_rgba(
                input_c.as_ptr(),
                empty.as_ptr(),
                0,
                64,
                64,
                &mut w,
                &mut h,
                &mut len,
            );
            assert!(!rgba.is_null(), "rgba render failed: {}", last_error());
            assert_eq!(len, w * h * 4);
            assert!(w <= 64 && h <= 64);
            pdf_free_buffer(rgba, len);

            // Out-of-range page reports an error rather than panicking.
            assert!(pdf_render_page_png(
                input_c.as_ptr(),
                empty.as_ptr(),
                99,
                64,
                64,
                &mut out_len
            )
            .is_null());
            assert!(last_error().starts_with("PAGE_OUT_OF_RANGE"));

            let (mut pw_pts, mut ph_pts) = (0.0f64, 0.0f64);
            assert_eq!(
                pdf_page_size(
                    input_c.as_ptr(),
                    empty.as_ptr(),
                    0,
                    &mut pw_pts,
                    &mut ph_pts
                ),
                0
            );
            assert!(pw_pts > 0.0 && ph_pts > 0.0);

            // --- Milestone 13: images -> PDF ---------------------------
            let jpeg_path = dir.join("page.jpg");
            std::fs::write(&jpeg_path, sample_jpeg()).unwrap();
            let images = c(&format!(
                "{}\n{}",
                jpeg_path.to_str().unwrap(),
                jpeg_path.to_str().unwrap()
            ));
            let composed = dir.join("composed.pdf");
            let composed_c = c(composed.to_str().unwrap());
            assert_eq!(
                pdf_images_to_pdf(images.as_ptr(), composed_c.as_ptr(), 0.0, 0.0, 1, 0.0),
                0,
                "compose failed: {}",
                last_error()
            );
            assert_eq!(pdf_page_count(composed_c.as_ptr(), empty.as_ptr()), 2);

            // And the composed document renders back.
            let png = pdf_render_page_png(
                composed_c.as_ptr(),
                empty.as_ptr(),
                0,
                80,
                80,
                &mut out_len,
            );
            assert!(!png.is_null(), "composed render failed: {}", last_error());
            pdf_free_buffer(png, out_len);
        }
    }

    unsafe fn last_error() -> String {
        CStr::from_ptr(pdf_last_error()).to_string_lossy().into_owned()
    }

    /// A 2x2 baseline greyscale JPEG, hand-assembled so the test needs no
    /// encoder dependency.
    fn sample_jpeg() -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // SOF0: 8-bit precision, 2x2, one component.
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x02, 0x01, 0x11, 0x00]);
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }
}
