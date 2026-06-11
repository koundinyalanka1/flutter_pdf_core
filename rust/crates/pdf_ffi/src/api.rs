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
use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use pdf_ai::chunker::ChunkOptions;
use pdf_core::crypt::encrypt_to_bytes;
use pdf_core::document::PdfDocument;
use pdf_core::error::PdfError;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
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
    PdfDocument::from_path_with_password(path, password)
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
        let doc = open(path, password)?;
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
        let doc = open(path, password)?;
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
        let doc = open(path, password)?;
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
        let doc = open(path, password)?;
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
    let (Ok(path), Ok(password)) = (cstr(path), cstr(password)) else {
        return std::ptr::null_mut();
    };
    run_str(|| {
        let doc = open(path, password)?;
        if page <= 0 {
            Ok(pdf_text::extractor::extract_all_pages(&doc)?.join("\u{0C}"))
        } else {
            pdf_text::extractor::extract_page_text(&doc, (page - 1) as usize)
        }
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
        let doc = open(path, password)?;
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
        let doc = open(path, password)?;
        let bytes = encrypt_to_bytes(&doc, user_pw, owner_pw)?;
        std::fs::write(out, bytes)?;
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
        let doc = open(path, password)?;
        doc.save_as(out)?; // the standard writer always writes decrypted
        Ok(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
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
        }
    }
}
