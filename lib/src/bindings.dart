// Milestone 11: raw dart:ffi bindings for the Rust pdf_ffi crate.
//
// All `Pointer<Utf8>` strings are UTF-8 and NUL-terminated. Strings returned
// by the library are freed with [PdfBindings.freeString]; `pdf_last_error`
// returns a borrowed pointer that must NOT be freed.

import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

// -- native signatures -------------------------------------------------------

typedef _VersionC = Pointer<Utf8> Function();
typedef _LastErrorC = Pointer<Utf8> Function();
typedef _FreeStringC = Void Function(Pointer<Utf8>);
typedef _FreeStringDart = void Function(Pointer<Utf8>);

typedef _PageCountC = Int32 Function(Pointer<Utf8>, Pointer<Utf8>);
typedef _PageCountDart = int Function(Pointer<Utf8>, Pointer<Utf8>);

typedef _Str2C = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>);

typedef _ExtractTextC = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>, Int32);
typedef _ExtractTextDart = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>, int);

typedef _ExportAiC = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Int32, Int32, Int32);
typedef _ExportAiDart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, int, int, int);

typedef _Op3C = Int32 Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _Op3Dart = int Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _Op4C = Int32 Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _Op4Dart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _Op5C = Int32 Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _Op5Dart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _MergeC = Int32 Function(Pointer<Utf8>, Pointer<Utf8>);
typedef _MergeDart = int Function(Pointer<Utf8>, Pointer<Utf8>);

typedef _RotateC = Int32 Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Int32, Pointer<Utf8>);
typedef _RotateDart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, int, Pointer<Utf8>);

typedef _CropC = Int32 Function(Pointer<Utf8>, Pointer<Utf8>, Int32, Double,
    Double, Double, Double, Pointer<Utf8>);
typedef _CropDart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, int, double, double, double, double, Pointer<Utf8>);

// Milestone 13: rendering + image composition.

typedef _RenderPngC = Pointer<Uint8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Int32, Int32, Int32, Pointer<Int32>);
typedef _RenderPngDart = Pointer<Uint8> Function(
    Pointer<Utf8>, Pointer<Utf8>, int, int, int, Pointer<Int32>);

typedef _RenderRgbaC = Pointer<Uint8> Function(Pointer<Utf8>, Pointer<Utf8>,
    Int32, Int32, Int32, Pointer<Int32>, Pointer<Int32>, Pointer<Int32>);
typedef _RenderRgbaDart = Pointer<Uint8> Function(Pointer<Utf8>, Pointer<Utf8>,
    int, int, int, Pointer<Int32>, Pointer<Int32>, Pointer<Int32>);

typedef _FreeBufferC = Void Function(Pointer<Uint8>, Int32);
typedef _FreeBufferDart = void Function(Pointer<Uint8>, int);

typedef _PageSizeC = Int32 Function(
    Pointer<Utf8>, Pointer<Utf8>, Int32, Pointer<Double>, Pointer<Double>);
typedef _PageSizeDart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, int, Pointer<Double>, Pointer<Double>);

typedef _ImagesToPdfC = Int32 Function(
    Pointer<Utf8>, Pointer<Utf8>, Double, Double, Int32, Double);
typedef _ImagesToPdfDart = int Function(
    Pointer<Utf8>, Pointer<Utf8>, double, double, int, double);

/// Lazily-resolved bindings to the native library.
class PdfBindings {
  PdfBindings._(DynamicLibrary lib)
      : version = lib.lookupFunction<_VersionC, _VersionC>('pdf_core_version'),
        lastError = lib.lookupFunction<_LastErrorC, _LastErrorC>('pdf_last_error'),
        freeString =
            lib.lookupFunction<_FreeStringC, _FreeStringDart>('pdf_free_string'),
        pageCount =
            lib.lookupFunction<_PageCountC, _PageCountDart>('pdf_page_count'),
        inspectJson = lib.lookupFunction<_Str2C, _Str2C>('pdf_inspect_json'),
        getMetadataJson =
            lib.lookupFunction<_Str2C, _Str2C>('pdf_get_metadata_json'),
        setMetadata = lib.lookupFunction<_Op4C, _Op4Dart>('pdf_set_metadata'),
        extractPages =
            lib.lookupFunction<_Op4C, _Op4Dart>('pdf_extract_pages'),
        deletePages = lib.lookupFunction<_Op4C, _Op4Dart>('pdf_delete_pages'),
        reorderPages =
            lib.lookupFunction<_Op4C, _Op4Dart>('pdf_reorder_pages'),
        merge = lib.lookupFunction<_MergeC, _MergeDart>('pdf_merge'),
        rotatePages = lib.lookupFunction<_RotateC, _RotateDart>('pdf_rotate_pages'),
        setCropBox = lib.lookupFunction<_CropC, _CropDart>('pdf_set_crop_box'),
        extractText =
            lib.lookupFunction<_ExtractTextC, _ExtractTextDart>('pdf_extract_text'),
        exportAi = lib.lookupFunction<_ExportAiC, _ExportAiDart>('pdf_export_ai'),
        encrypt = lib.lookupFunction<_Op5C, _Op5Dart>('pdf_encrypt'),
        renderPagePng =
            lib.lookupFunction<_RenderPngC, _RenderPngDart>('pdf_render_page_png'),
        renderPageRgba = lib
            .lookupFunction<_RenderRgbaC, _RenderRgbaDart>('pdf_render_page_rgba'),
        freeBuffer =
            lib.lookupFunction<_FreeBufferC, _FreeBufferDart>('pdf_free_buffer'),
        pageSize = lib.lookupFunction<_PageSizeC, _PageSizeDart>('pdf_page_size'),
        imagesToPdf =
            lib.lookupFunction<_ImagesToPdfC, _ImagesToPdfDart>('pdf_images_to_pdf'),
        decrypt = lib.lookupFunction<_Op3C, _Op3Dart>('pdf_decrypt');

  final _VersionC version;
  final _LastErrorC lastError;
  final _FreeStringDart freeString;
  final _PageCountDart pageCount;
  final _Str2C inspectJson;
  final _Str2C getMetadataJson;
  final _Op4Dart setMetadata;
  final _Op4Dart extractPages;
  final _Op4Dart deletePages;
  final _Op4Dart reorderPages;
  final _MergeDart merge;
  final _RotateDart rotatePages;
  final _CropDart setCropBox;
  final _ExtractTextDart extractText;
  final _ExportAiDart exportAi;
  final _Op5Dart encrypt;
  final _Op3Dart decrypt;
  final _RenderPngDart renderPagePng;
  final _RenderRgbaDart renderPageRgba;
  final _FreeBufferDart freeBuffer;
  final _PageSizeDart pageSize;
  final _ImagesToPdfDart imagesToPdf;

  static PdfBindings? _instance;

  /// Resolve the bindings, loading the dynamic library for this platform.
  ///
  /// Override the library location with the `PDF_CORE_LIB_PATH` environment
  /// variable (useful for tests and desktop development).
  static PdfBindings get instance => _instance ??= PdfBindings._(_open());

  static DynamicLibrary _open() {
    final override = Platform.environment['PDF_CORE_LIB_PATH'];
    if (override != null && override.isNotEmpty) {
      return DynamicLibrary.open(override);
    }
    if (Platform.isAndroid) {
      return DynamicLibrary.open('libpdf_ffi.so');
    }
    if (Platform.isIOS || Platform.isMacOS) {
      // Statically linked into the app binary (see podspecs).
      try {
        return DynamicLibrary.process();
      } on ArgumentError {
        return DynamicLibrary.executable();
      }
    }
    if (Platform.isWindows) {
      return DynamicLibrary.open('pdf_ffi.dll');
    }
    // Linux and friends.
    return DynamicLibrary.open('libpdf_ffi.so');
  }
}
