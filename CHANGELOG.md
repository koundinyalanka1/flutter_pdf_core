## Unreleased

* Pages render about twice as fast on a phone, and zooming in again on a
  page already shown is many times faster, for every kind of document.
  Measured on a Motorola edge 70 fusion at its screen width: an AutoCAD
  parking plan (155,000 paths) 2.4 s → 1.1 s for the page and 1.7 s →
  0.8 s for a zoomed region before the document cache below; a scanned
  letter 325 → 190 ms and 400 → 205 ms. Over the 958 pages of the test
  corpus on a Mac: 30.7 s on one thread, 14.6 s in bands, 6.5 s rendering
  again from a kept document. Where the time went:
  * A render is cut into horizontal bands drawn on up to four threads
    (`RenderOptions::threads`; 0, the default, uses the device's cores up
    to four, 1 draws on the caller's thread). The bands share the page's
    parsed content, its fonts and its decoded images, so a full-page scan
    is decoded once, not once per band, and a render keeps the images it
    has used for its later bands; they split the scratch-memory budget one
    render had. More than four threads made dense drawings slower on both
    phones measured.
  * A document can keep what its renders derive from it —
    `PdfDocument::derived`, a `pdf_core::derived::DerivedCache`, off unless
    enabled and only for a document no longer changed. `pdf_document_open`
    enables it for the documents the app pins while they are open, so the
    next render of a page, zoomed in further, decodes no image and parses
    nothing again: a zoomed region of a scanned page 36 → 6 ms on a Mac, of
    an Internet Archive book page 660 → 20 ms. Bounded: 64 MB of decoded
    images, 32 MB of parsed pages, 16 MB of parsed forms, the least
    recently used let go first.
  * Content streams are parsed into `pdf_text::content_stream::Content`:
    operators and operands in a few flat arrays, 12 bytes an operator and 8
    an operand, without an allocation per operator. The plan's stream
    (725,000 operators) took about 106 MB as `Operation`s; it takes 16,
    parses faster and frees at once. `parse_content` is unchanged for text extraction; both share one
    parser and recover from damage alike.
  * Gray images are held at a byte a pixel rather than as RGB
    (`DecodedImage::gray`): a scanned book page with its text mask 101 MB →
    49 MB, decoded and sampled faster.
  * Nothing is parsed twice in one render: a tiling pattern's content once
    for all its tiles, not once a tile; a Type3 glyph's procedure the first
    time the glyph is drawn, kept with its font; a form XObject once, kept
    by object.
  * Paths, glyphs, forms and images wholly outside the canvas are skipped
    before any work: a path from the corners of its bounds before it is
    transformed, a glyph from a box four ems around it before its outline
    is built, a form before its clip is made or its content parsed, an
    image before it is decoded. A zoomed-in region costs what is in it, not
    the whole page.
  * Far fewer allocations, which on Android's allocator are slow and
    contend between threads: 7.5 million → 3.5 million for the plan even
    before compact content. Edge and active-edge lists are reused between
    fills; stroke pieces are pushed whole; solid strokes are not copied; a
    content stream's paths reuse their point buffers; the operators PDF
    defines are held as an index (`pdf_core::lexer::Keyword`, now the type
    of `Token::Keyword` and `Operation::operator`), found by first byte
    rather than compared against all of them.
  * Rendered PNGs use zlib level 1 rather than 6: five times faster (60
    rather than 320 ms for a zoomed region of the plan), a third larger,
    and they only cross to the app's own decoder. A zoomed-in region need
    not be PNG at all: `pdf_render_page_region_rgba`
    (`PdfCore.renderPageRegionRgbaWithWarnings`,
    `PdfCore.canRenderRegionPixels`) returns its raw pixels.
  * Bands produce the same pixels as one thread, and a kept document the
    same as a fresh one: all 958 corpus pages compared four ways, identical
    but for one pdf.js test page whose tiling pattern one thread now gives
    up on and bands draw. A tiling pattern is judged against the work limit
    on its own, for the whole region; across a page's patterns each band is
    stopped at four times that.
* A text object with a `gs` inside it is drawn through a page-sized
  knockout buffer only when that `gs` makes it transparent. AutoCAD sets an
  opaque state inside every text object; at large sizes the buffers ran
  past the memory limit and the text was left out.
* A content stream or CMap with an unmatched `)`, `>`, `{` or `}` no longer
  hangs: the lexer returned an empty keyword without moving on, which the
  content parser took as an operator forever, filling memory until the
  process was killed (a fuzzed pdf.js test file did so in under a second).
* Rectangle and stroke snapping rounds halves up rather than away from
  zero, so a zoomed-in region or a band, whose coordinates are the page's
  shifted by whole pixels, snaps exactly as the whole page does. The
  tiling-pattern work limit is judged for the whole region, from a new
  record of the clip's bounds, so every band of a page decides alike.
* Gradients whose colour space is `ICCBased`, `CalRGB` or `CalGray` are
  drawn; they were skipped, so a gradient in a page written by macOS
  (Quartz) was missing.
* A substitute for a font the document did not embed matches its weight, not
  just bold or regular: the weight comes from style words in the name
  (`Light`, `Medium`, `Heavy`, ...) or a Japanese `W0`–`W9` grade, and the
  nearest weight of the chosen family wins, so a `GothicBBB-Medium` heading
  is not set in the family's hairline `W0` face. More Japanese family names
  (`GothicBBB`, `FutoGo`, `FutoMin`, `Midashi`, `Jun101`, `KozGo`, `KozMin`)
  pick a gothic or mincho substitute. Found, with the rectangle rounding
  below, by comparing pages with Adobe Acrobat; most pages of the test corpus
  now differ from Acrobat's render by 0.2–3 grey levels on average.
* Vertical CJK text in fonts the document did not embed uses the vertical
  forms the substitute face carries (its `GSUB` `vert` feature), as other
  viewers do: 、 and 。 sit at the top right of their cells, brackets turn on
  their side, ー runs down the page. CJK punctuation now comes from the
  device's CJK face rather than a bundled Yi or Mongolian face, which also
  carries 、 and 。 but in its own design and without vertical forms. Found by
  comparing renders with Apple's PDF engine and MuPDF.
* Images honour `/Mask`. Explicit masks (an image mask that says where the
  image shows) were ignored, so a scanned book built as a coloured layer seen
  through a sharp text mask, as every Internet Archive scan is, painted the
  whole layer: a grey page streaked with the colour smeared between the
  letters. Colour-key masks (`/Mask [min max ...]`) make matching samples
  transparent. A mask or `/SMask` of another size than its image is sampled at
  its own resolution, so a fine text mask over coarse colour stays sharp.
* Sharper rendering, measured against PDFium (what Chrome and Drive use) at
  phone resolution on Latin, Indic, CJK and scanned pages:
  * Images are area-sampled. Each device pixel averages the part of the image
    it covers; before, it took one source pixel, so a 300 dpi scan shown at
    phone width lost most of its pixels and thin strokes broke up. Enlarged
    images keep crisp pixel edges unless `/Interpolate` asks for smoothing,
    as in PDFium. Upright, flipped and quarter-turned images use an exact
    separable filter; others are supersampled, with smooth edges.
  * A filled rectangle on its own (a table border, an underline, a cell
    background) lands on whole pixels, at least one wide, its size rounded
    as Adobe Acrobat rounds it (PDFium rounds up, which drew a 1.45-pixel
    border two pixels thick), and rectangular clips widen to whole pixels. Strokes are never thinner
    than one device pixel, and thin horizontal and vertical strokes snap to
    the pixel grid (automatic stroke adjustment). A 0.25 pt rule on a phone
    is now a solid line instead of a faint grey one.
  * Anti-aliasing samples 16 sub-scanlines per row instead of 4, so hairline
    horizontal strokes no longer drop out between samples. Only edges that
    cross a row, and only the pixels it touched, are visited, which keeps
    rendering as fast as before.
  * Partly covered pixels of glyph outlines are drawn a little darker
    (coverage to the power 0.9), matching the weight of PDFium's hinted text
    to within 1%; solid pixels and large text are unchanged.
* Render part of a page: `RenderOptions::region`, `pdf_render_page_region_png`
  (`PdfCore.renderPageRegionPngWithWarnings`, `PdfCore.canRenderRegions`).
  The page is laid out at the requested size and only the region is drawn
  and allocated, so a viewer can keep a zoomed page sharp with a screenful of
  pixels instead of a full-page bitmap of hundreds of megabytes.
* OCR, written from scratch (new `pdf_ocr` crate). Recognizes printed
  Latin-script text on scanned pages. Pages scanned skewed, sideways or upside
  down are handled. `pdf_ocr_page_json` (`PdfCore.ocrPage`) reads a page and
  returns its lines, word boxes and selectable layout.
  `pdf_make_searchable_json` (`PdfCore.makeSearchable`) saves a copy with an
  invisible text layer over every scanned page, and `pdf_apply_ocr_json`
  (`PdfCore.applyOcr`) writes results recognized page by page or by another
  engine. Pages that already carry text are left alone unless forced. The
  layer renders invisibly, its word boxes match the scan, and text
  extraction, selection, search and AI export read it like any other text.
  The recognizer's model (about 1 MB) is compiled in; `tools/ocr_train`
  reproduces it.
* OCR leaves born-digital pages with only a little text alone, such as a
  cover or divider page. Before, any page with fewer than 50 characters was
  recognized, and its text was written a second time ("Chapter One" extracted
  as "Chapter OneChapter One"). A few characters over a page-sized image, as
  on a scan with a page number stamped on, are still recognized.
  `page_text_stats` reports the share of the page images cover
  (`image_coverage`) to tell the two apart.
* Plain-text extraction (`pdf_extract_text`, AI export) now places spaces and
  line breaks along each run's baseline, as text layout already did. Rotated
  text no longer breaks into one line per run. Words set with a unit font
  size and a scaled text matrix no longer gain spaces at kerning gaps.
* `test/pdf_ocr_native_test.dart` runs OCR from Dart against a real native
  library when `PDF_CORE_LIB_PATH` is set.
* OCR reads a page in reading order even when its lines are long: long lines
  of small print are split only after lines are ordered, and only beyond 150
  line heights.
* Text extraction tracks the text rendering mode; `page_text_stats` counts a
  page's visible, invisible and unmapped text.
* `MacRomanEncoding` decodes the whole high range. Before, the fi and fl
  ligatures that macOS writes for "file" or "flat" came out as "Þ" and "ß".
* Pin parsed documents: `pdf_document_open`/`pdf_document_close`
  (`PdfCore.openDocument`/`closeDocument`) keep one parse in memory, and
  read-only calls on the same path and password share it while the file is
  unchanged, instead of each reading and parsing the whole file. Viewers that
  make many calls at once no longer multiply memory use by their concurrency.
* Read common writer quirks as intact files instead of recovering them as
  damaged: index entries in use at offset 0 are free, `endobj` may be omitted
  before the next object, the index or the end of the file, integers too large
  for 64 bits saturate, and references to impossible object numbers read as
  null (they still never wrap onto a real object).
* Encode passwords as each revision requires: PDFDocEncoding for RC4/AES-128
  (revisions 2–4), UTF-8 after SASLprep for AES-256, with the raw UTF-8 bytes
  as a fallback. AES-256 encryption applies SASLprep; files encrypted by
  earlier versions still open. Adds the `stringprep` dependency.
* Enforce render pixel budgets for extreme page sizes and aspect ratios;
  honour tiny fit boxes and reject invalid scales or zero pixel budgets.
* Resolve indirect page rotations consistently in rendering and size queries.
* Treat null page attributes as absent when inheriting settings, including
  when rebuilding a page tree after page operations.
* Reject non-finite crop/media boxes and normalize large rotation deltas
  before addition to avoid integer overflow.
* Reject out-of-range PDF object numbers and generations instead of truncating
  them to references to different objects.
* Return `PAGE_OUT_OF_RANGE` for negative page indices at the C/Flutter
  boundary. Failed renders clear output lengths, dimensions, and prior warnings.
* Keep AI chunks within their Unicode character limit, including overlap,
  and retain the source-page range of overlapping text. Rust chunk limits
  below 64 are now honoured; zero is treated as one character.
* Reject objects nested more than 128 levels deep instead of overflowing the
  stack, which aborts the process even behind the FFI panic guard.
* Write output files atomically, including `pdf_encrypt`: a failed save
  leaves an existing destination untouched.
* Link Android libraries for 16 KB memory pages (Android 15+), and build
  them with `--locked`.
* Export `PdfRenderedPng` from the package library.

These changes extend the C ABI with the two document-pinning functions and
the three OCR functions. The Dart API gains `openDocument`, `closeDocument`,
the `PdfRenderedPng` export, and `ocrPage`, `makeSearchable(Async)` and
`applyOcr(Async)` with their `PdfOcr…` types. Bindings treat the new functions
as optional, so older native libraries keep working; OCR calls then throw
`OCR_UNAVAILABLE`. The checked-in Android, iOS and macOS binaries have been
regenerated from this source, OCR model included.

## 0.1.0

Milestones 3–12 of the from-scratch Rust PDF core:

* Page tree model with inheritance + clean rebuilds (M3)
* Split / delete / reorder / duplicate pages with compact renumbering (M4)
* Merge any number of documents (M5)
* Rotate, media/crop boxes, full /Info metadata editing incl. UTF-16 (M6)
* FlateDecode (+PNG/TIFF predictors), ASCIIHex/85, xref streams, /Prev
  chains, hybrid files, object streams (M7)
* Text extraction: text-state machine, WinAnsi/MacRoman/Standard +
  /Differences, ToUnicode CMaps, Identity-H CID fonts, form XObjects (M8)
* AI-ready JSON/NDJSON export with paragraph-aware overlapping chunks (M9)
* Encryption: opens RC4 40/128, AES-128, AES-256 (R5/R6) files; writes
  AES-256 (R6, PDF 2.0); decrypt-on-save (M10)
* Flutter integration via hand-written C ABI + dart:ffi, sync + isolate
  async APIs, typed PdfException codes (M11)
* Android (cargo-ndk), iOS (XCFramework), macOS (universal dylib) build
  scripts and GitHub Actions CI (M12)

## 0.0.1

* Initial plugin scaffold; Rust parser/inspector (M1) and writer with
  round-trip tests (M2).
