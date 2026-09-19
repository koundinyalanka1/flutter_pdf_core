// Milestone 11: the public Dart API over the Rust core.

import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'bindings.dart';

/// Thrown when a native PDF operation fails.
class PdfException implements Exception {
  PdfException(this.code, this.message);

  /// Stable machine-readable code: `ENCRYPTED`, `WRONG_PASSWORD`,
  /// `NOT_A_PDF`, `PAGE_OUT_OF_RANGE`, `ERROR`, `PANIC`.
  final String code;
  final String message;

  bool get isEncrypted => code == 'ENCRYPTED';
  bool get isWrongPassword => code == 'WRONG_PASSWORD';

  @override
  String toString() => 'PdfException($code): $message';
}

/// A rendered page and anything the renderer had to leave out of it.
///
/// A page can render successfully and still not be a faithful copy — an image
/// in a codec this build does not read, or text in a font the document never
/// embedded and which is therefore drawn in a substitute face. The warnings
/// let a caller say so rather than presenting an approximation as exact.
class PdfRenderedPng {
  const PdfRenderedPng(this.bytes, this.warnings);

  final Uint8List bytes;

  /// Human-readable notes, empty when the page rendered exactly as authored.
  final List<String> warnings;

  bool get isApproximate => warnings.isNotEmpty;
}

/// Document metadata (all fields optional).
///
/// When passed to [PdfCore.setMetadata]: `null` leaves a field untouched,
/// an empty string deletes it.
class PdfMetadata {
  const PdfMetadata({
    this.title,
    this.author,
    this.subject,
    this.keywords,
    this.creator,
    this.producer,
    this.creationDate,
    this.modDate,
  });

  factory PdfMetadata.fromJson(Map<String, dynamic> json) => PdfMetadata(
        title: json['title'] as String?,
        author: json['author'] as String?,
        subject: json['subject'] as String?,
        keywords: json['keywords'] as String?,
        creator: json['creator'] as String?,
        producer: json['producer'] as String?,
        creationDate: json['creation_date'] as String?,
        modDate: json['mod_date'] as String?,
      );

  final String? title;
  final String? author;
  final String? subject;
  final String? keywords;
  final String? creator;
  final String? producer;
  final String? creationDate;
  final String? modDate;

  Map<String, dynamic> toJson() => {
        'title': title,
        'author': author,
        'subject': subject,
        'keywords': keywords,
        'creator': creator,
        'producer': producer,
        'creation_date': creationDate,
        'mod_date': modDate,
      };
}

/// Summary information about a document.
class PdfInfo {
  const PdfInfo({
    required this.version,
    required this.encrypted,
    required this.objectCount,
    required this.pageCount,
    required this.metadata,
  });

  factory PdfInfo.fromJson(Map<String, dynamic> json) => PdfInfo(
        version: json['version'] as String? ?? '',
        encrypted: json['encrypted'] as bool? ?? false,
        objectCount: json['objectCount'] as int? ?? 0,
        pageCount: json['pageCount'] as int? ?? 0,
        metadata: PdfMetadata.fromJson(
            (json['metadata'] as Map?)?.cast<String, dynamic>() ?? const {}),
      );

  final String version;
  final bool encrypted;
  final int objectCount;
  final int pageCount;
  final PdfMetadata metadata;
}

/// A rasterized page: RGBA8 pixels plus their dimensions.
///
/// The buffer is straight (non-premultiplied) RGBA with a top-left origin,
/// which is exactly what `ui.decodeImageFromPixels` expects.
class PdfRenderedPage {
  const PdfRenderedPage({
    required this.width,
    required this.height,
    required this.pixels,
  });

  final int width;
  final int height;
  final Uint8List pixels;
}

/// A page's size in PostScript points (1/72"), honouring `/Rotate`.
class PdfPageSize {
  const PdfPageSize(this.width, this.height);
  final double width;
  final double height;

  double get aspectRatio => height == 0 ? 1 : width / height;
}

/// How an image is placed on its page by [PdfCore.imagesToPdf].
enum PdfImageFit {
  /// Fixed page size; the image is scaled to fit and centred (letterboxed).
  contain,

  /// The page takes the image's own aspect ratio — no bars.
  imageAspect,
}

/// Pure-Rust PDF toolkit: parse, split, merge, rotate, crop, metadata,
/// text extraction, rendering, AI export and AES-256 password protection.
///
/// All methods are synchronous FFI calls; the `xxxAsync` variants run the
/// same call on a background isolate so heavy documents don't jank the UI.
///
/// Page selections use 1-based range strings such as `'1-3,5'`.
/// An empty selection string means "all pages".
class PdfCore {
  PdfCore._();

  static PdfBindings get _b => PdfBindings.instance;

  /// Native library version.
  static String get nativeVersion => _b.version().toDartString();

  // -- queries ---------------------------------------------------------------

  static int pageCount(String path, {String password = ''}) =>
      _int2(_b.pageCount, path, password);

  static PdfInfo inspect(String path, {String password = ''}) {
    final json = _str2(_b.inspectJson, path, password);
    return PdfInfo.fromJson(jsonDecode(json) as Map<String, dynamic>);
  }

  static PdfMetadata getMetadata(String path, {String password = ''}) {
    final json = _str2(_b.getMetadataJson, path, password);
    return PdfMetadata.fromJson(jsonDecode(json) as Map<String, dynamic>);
  }

  /// Extracted text. With [page] (1-based) extracts a single page;
  /// otherwise all pages, separated by form-feed (`\f`).
  static String extractText(String path, {int? page, String password = ''}) {
    return _withUtf8([path, password], (args) {
      final out = _b.extractText(args[0], args[1], page ?? 0);
      return _takeString(out);
    });
  }

  /// AI-ready export: pages + metadata + overlapping text chunks.
  /// Returns a JSON document, or NDJSON lines when [ndjson] is true.
  static String exportForAi(
    String path, {
    String password = '',
    int maxChars = 2000,
    int overlap = 200,
    bool ndjson = false,
  }) {
    return _withUtf8([path, password], (args) {
      final out = _b.exportAi(args[0], args[1], maxChars, overlap, ndjson ? 1 : 0);
      return _takeString(out);
    });
  }

  // -- transformations -------------------------------------------------------

  /// Copy [pages] (e.g. `'1-3,7'`) into a new file.
  static void extractPages(String path, String pages, String outPath,
          {String password = ''}) =>
      _check(_int4(_b.extractPages, path, password, pages, outPath));

  /// Delete [pages] and save the remainder.
  static void deletePages(String path, String pages, String outPath,
          {String password = ''}) =>
      _check(_int4(_b.deletePages, path, password, pages, outPath));

  /// Reorder pages; [order] must mention every page exactly once (`'3,1,2'`).
  static void reorderPages(String path, String order, String outPath,
          {String password = ''}) =>
      _check(_int4(_b.reorderPages, path, password, order, outPath));

  /// Merge [paths] into one document.
  static void merge(List<String> paths, String outPath) {
    if (paths.isEmpty) {
      throw PdfException('ERROR', 'no input files');
    }
    _check(_withUtf8([paths.join('\n'), outPath], (args) {
      return _b.merge(args[0], args[1]);
    }));
  }

  /// Rotate [pages] (empty = all) by [degrees] (multiple of 90).
  static void rotatePages(String path, int degrees, String outPath,
      {String pages = '', String password = ''}) {
    _check(_withUtf8([path, password, pages, outPath], (args) {
      return _b.rotatePages(args[0], args[1], args[2], degrees, args[3]);
    }));
  }

  /// Set the crop box of one [page] (1-based) in points.
  static void setCropBox(
    String path,
    int page,
    double x0,
    double y0,
    double x1,
    double y1,
    String outPath, {
    String password = '',
  }) {
    _check(_withUtf8([path, password, outPath], (args) {
      return _b.setCropBox(args[0], args[1], page, x0, y0, x1, y1, args[2]);
    }));
  }

  /// Write metadata; see [PdfMetadata] for null/empty semantics.
  static void setMetadata(String path, PdfMetadata metadata, String outPath,
      {String password = ''}) {
    final json = jsonEncode(metadata.toJson());
    _check(_int4(_b.setMetadata, path, password, json, outPath));
  }

  /// Password-protect with AES-256 (PDF 2.0). An empty [ownerPassword]
  /// reuses [userPassword].
  static void encrypt(String path, String userPassword, String outPath,
      {String ownerPassword = '', String password = ''}) {
    _check(_withUtf8([path, password, userPassword, ownerPassword, outPath],
        (args) {
      return _b.encrypt(args[0], args[1], args[2], args[3], args[4]);
    }));
  }

  /// Remove encryption (requires the correct [password]).
  static void decrypt(String path, String password, String outPath) =>
      _check(_int3(_b.decrypt, path, password, outPath));

  // -- rendering ---------------------------------------------------------

  /// Rasterize [page] (0-based) and return it as PNG bytes.
  ///
  /// When [width] and [height] are both positive the page is scaled to fit
  /// inside that pixel box, preserving aspect ratio; otherwise it renders at
  /// its natural size (72 dpi).
  static Uint8List renderPagePng(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) {
    return _withUtf8([path, password], (args) {
      final lengthOut = calloc<Int32>();
      try {
        final buffer =
            _b.renderPagePng(args[0], args[1], page, width, height, lengthOut);
        if (buffer == nullptr) throw _lastError();
        return _takeBuffer(buffer, lengthOut.value);
      } finally {
        calloc.free(lengthOut);
      }
    });
  }

  /// As [renderPagePng], but also reports what the renderer had to skip.
  ///
  /// The warnings come from a thread-local the native side fills in during the
  /// render, so they are read here, immediately after the call that set them.
  static PdfRenderedPng renderPagePngWithWarnings(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) {
    final bytes =
        renderPagePng(path, page, width: width, height: height, password: password);
    return PdfRenderedPng(bytes, _lastWarnings());
  }

  /// Rasterize [page] (0-based) to raw RGBA8 — the cheapest path to a
  /// `ui.Image`, with no encode/decode round trip.
  static PdfRenderedPage renderPageRgba(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) {
    return _withUtf8([path, password], (args) {
      final widthOut = calloc<Int32>();
      final heightOut = calloc<Int32>();
      final lengthOut = calloc<Int32>();
      try {
        final buffer = _b.renderPageRgba(
          args[0],
          args[1],
          page,
          width,
          height,
          widthOut,
          heightOut,
          lengthOut,
        );
        if (buffer == nullptr) throw _lastError();
        return PdfRenderedPage(
          width: widthOut.value,
          height: heightOut.value,
          pixels: _takeBuffer(buffer, lengthOut.value),
        );
      } finally {
        calloc.free(widthOut);
        calloc.free(heightOut);
        calloc.free(lengthOut);
      }
    });
  }

  /// Page size in points, honouring `/Rotate`.
  static PdfPageSize pageSize(String path, int page, {String password = ''}) {
    return _withUtf8([path, password], (args) {
      final widthOut = calloc<Double>();
      final heightOut = calloc<Double>();
      try {
        final status =
            _b.pageSize(args[0], args[1], page, widthOut, heightOut);
        _check(status);
        return PdfPageSize(widthOut.value, heightOut.value);
      } finally {
        calloc.free(widthOut);
        calloc.free(heightOut);
      }
    });
  }

  // -- composition -------------------------------------------------------

  /// Build a PDF from JPEG files, one page per image.
  ///
  /// The JPEG bytes are embedded as-is (`DCTDecode`), so there is no decode /
  /// re-encode step and no quality loss. [pageSizePoints] defaults to A4 and
  /// only matters for [PdfImageFit.contain]; under [PdfImageFit.imageAspect]
  /// it sets the target long edge.
  static void imagesToPdf(
    List<String> jpegPaths,
    String outPath, {
    PdfImageFit fit = PdfImageFit.imageAspect,
    PdfPageSize? pageSizePoints,
    double margin = 0,
  }) {
    if (jpegPaths.isEmpty) {
      throw PdfException('ERROR', 'no input images');
    }
    _check(_withUtf8([jpegPaths.join('\n'), outPath], (args) {
      return _b.imagesToPdf(
        args[0],
        args[1],
        pageSizePoints?.width ?? 0,
        pageSizePoints?.height ?? 0,
        fit == PdfImageFit.contain ? 0 : 1,
        margin,
      );
    }));
  }

  // -- async variants ----------------------------------------------------------

  static Future<int> pageCountAsync(String path, {String password = ''}) =>
      Isolate.run(() => pageCount(path, password: password));

  static Future<PdfInfo> inspectAsync(String path, {String password = ''}) =>
      Isolate.run(() => inspect(path, password: password));

  static Future<String> extractTextAsync(String path,
          {int? page, String password = ''}) =>
      Isolate.run(() => extractText(path, page: page, password: password));

  static Future<String> exportForAiAsync(
    String path, {
    String password = '',
    int maxChars = 2000,
    int overlap = 200,
    bool ndjson = false,
  }) =>
      Isolate.run(() => exportForAi(path,
          password: password,
          maxChars: maxChars,
          overlap: overlap,
          ndjson: ndjson));

  static Future<void> extractPagesAsync(String path, String pages, String outPath,
          {String password = ''}) =>
      Isolate.run(() => extractPages(path, pages, outPath, password: password));

  static Future<void> deletePagesAsync(String path, String pages, String outPath,
          {String password = ''}) =>
      Isolate.run(() => deletePages(path, pages, outPath, password: password));

  static Future<void> reorderPagesAsync(String path, String order, String outPath,
          {String password = ''}) =>
      Isolate.run(() => reorderPages(path, order, outPath, password: password));

  static Future<void> mergeAsync(List<String> paths, String outPath) =>
      Isolate.run(() => merge(paths, outPath));

  static Future<void> rotatePagesAsync(String path, int degrees, String outPath,
          {String pages = '', String password = ''}) =>
      Isolate.run(() => rotatePages(path, degrees, outPath,
          pages: pages, password: password));

  static Future<void> encryptAsync(String path, String userPassword, String outPath,
          {String ownerPassword = '', String password = ''}) =>
      Isolate.run(() => encrypt(path, userPassword, outPath,
          ownerPassword: ownerPassword, password: password));

  static Future<void> decryptAsync(String path, String password, String outPath) =>
      Isolate.run(() => decrypt(path, password, outPath));

  static Future<Uint8List> renderPagePngAsync(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) =>
      Isolate.run(() => renderPagePng(path, page,
          width: width, height: height, password: password));

  /// [renderPagePngWithWarnings] on a background isolate.
  ///
  /// Both the render and the warning read happen inside the isolate, which is
  /// the only place the thread-local they live in is valid.
  static Future<PdfRenderedPng> renderPagePngWithWarningsAsync(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) =>
      Isolate.run(() => renderPagePngWithWarnings(path, page,
          width: width, height: height, password: password));

  static Future<PdfRenderedPage> renderPageRgbaAsync(
    String path,
    int page, {
    int width = 0,
    int height = 0,
    String password = '',
  }) =>
      Isolate.run(() => renderPageRgba(path, page,
          width: width, height: height, password: password));

  static Future<PdfPageSize> pageSizeAsync(String path, int page,
          {String password = ''}) =>
      Isolate.run(() => pageSize(path, page, password: password));

  static Future<void> imagesToPdfAsync(
    List<String> jpegPaths,
    String outPath, {
    PdfImageFit fit = PdfImageFit.imageAspect,
    PdfPageSize? pageSizePoints,
    double margin = 0,
  }) =>
      Isolate.run(() => imagesToPdf(jpegPaths, outPath,
          fit: fit, pageSizePoints: pageSizePoints, margin: margin));

  // -- plumbing ----------------------------------------------------------------

  static R _withUtf8<R>(List<String> strings, R Function(List<Pointer<Utf8>>) f) {
    final pointers = strings.map((s) => s.toNativeUtf8()).toList();
    try {
      return f(pointers);
    } finally {
      for (final p in pointers) {
        malloc.free(p);
      }
    }
  }

  /// Copy a native buffer into a Dart list and release the native memory.
  static Uint8List _takeBuffer(Pointer<Uint8> ptr, int length) {
    try {
      if (length <= 0) return Uint8List(0);
      // asTypedList is a view over native memory; copy before freeing.
      return Uint8List.fromList(ptr.asTypedList(length));
    } finally {
      _b.freeBuffer(ptr, length);
    }
  }

  static String _takeString(Pointer<Utf8> ptr) {
    if (ptr == nullptr) {
      throw _lastError();
    }
    try {
      return ptr.toDartString();
    } finally {
      _b.freeString(ptr);
    }
  }

  static int _int2(int Function(Pointer<Utf8>, Pointer<Utf8>) f, String a, String b) {
    final result = _withUtf8([a, b], (args) => f(args[0], args[1]));
    if (result < 0) {
      throw _lastError();
    }
    return result;
  }

  static int _int3(int Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>) f,
          String a, String b, String c) =>
      _withUtf8([a, b, c], (args) => f(args[0], args[1], args[2]));

  static int _int4(
          int Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>) f,
          String a,
          String b,
          String c,
          String d) =>
      _withUtf8([a, b, c, d], (args) => f(args[0], args[1], args[2], args[3]));

  static String _str2(
      Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>) f, String a, String b) {
    return _withUtf8([a, b], (args) => _takeString(f(args[0], args[1])));
  }

  static void _check(int status) {
    if (status != 0) {
      throw _lastError();
    }
  }

  static List<String> _lastWarnings() {
    final raw = _b.lastWarnings().toDartString();
    if (raw.isEmpty) return const [];
    return raw.split('\n').where((line) => line.isNotEmpty).toList();
  }

  static PdfException _lastError() {
    final raw = _b.lastError().toDartString();
    final colon = raw.indexOf(':');
    if (colon > 0) {
      return PdfException(
          raw.substring(0, colon), raw.substring(colon + 1).trim());
    }
    return PdfException('ERROR', raw.isEmpty ? 'unknown error' : raw);
  }
}
