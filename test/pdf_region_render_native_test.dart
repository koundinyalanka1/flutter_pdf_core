// Region rendering through the real native library, from Dart. Skipped unless
// PDF_CORE_LIB_PATH names a built library, for example:
//
//   PDF_CORE_LIB_PATH=macos/Frameworks/libpdf_ffi.dylib flutter test test/pdf_region_render_native_test.dart
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

(int, int) pngSize(Uint8List png) {
  final data = ByteData.sublistView(png);
  return (data.getUint32(16), data.getUint32(20));
}

void main() {
  final library = Platform.environment['PDF_CORE_LIB_PATH'];
  final skip = library == null || library.isEmpty ? 'set PDF_CORE_LIB_PATH to run' : false;
  const pdf = 'test/sample.pdf';

  test('a region is the matching part of the page, clipped to it', () async {
    expect(PdfCore.canRenderRegions, isTrue);
    final full = PdfCore.renderPagePngWithWarnings(pdf, 0, width: 300, height: 3000);
    final (width, height) = pngSize(full.bytes);
    expect(width, 300);

    final part = await PdfCore.renderPageRegionPngWithWarningsAsync(
      pdf,
      0,
      pageWidth: 300,
      pageHeight: 3000,
      x: 250,
      y: 20,
      width: 100,
      height: 40,
    );
    // Hanging off the right edge, so only the 50 pixels on the page come back.
    expect(pngSize(part.bytes), (50, 40));

    expect(
      () => PdfCore.renderPageRegionPngWithWarnings(
        pdf,
        0,
        pageWidth: 300,
        pageHeight: 3000,
        x: 0,
        y: height,
        width: 10,
        height: 10,
      ),
      throwsA(isA<PdfException>()),
    );
  }, skip: skip);
}
