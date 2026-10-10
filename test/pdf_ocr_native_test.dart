// OCR through the real native library, from Dart. Skipped unless
// PDF_CORE_LIB_PATH names a built library, for example:
//
//   PDF_CORE_LIB_PATH=macos/Frameworks/libpdf_ffi.dylib flutter test test/pdf_ocr_native_test.dart
import 'dart:io';

import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  final library = Platform.environment['PDF_CORE_LIB_PATH'];
  final skip = library == null || library.isEmpty ? 'set PDF_CORE_LIB_PATH to run' : false;

  test('a scan is read, made searchable, and then extracts as text', () async {
    final dir = await Directory.systemTemp.createTemp('pdf_ocr_native');
    try {
      const scan = 'rust/fixtures/scanned.pdf';
      expect(PdfCore.extractText(scan).trim(), isEmpty);

      final page = await PdfCore.ocrPage(scan, page: 1);
      expect(page.status, PdfOcrStatus.recognized);
      expect(page.text, contains('Scanned invoice no. 2024-117'));
      expect(page.text, contains(r'$1,234.56'));
      expect(page.layout!.hasText, isTrue);

      final out = '${dir.path}/searchable.pdf';
      final report = await PdfCore.makeSearchableAsync(scan, out);
      expect(report.recognized, 1);
      expect(PdfCore.extractText(out), contains('Thank you for your business!'));
      final layout = await PdfCore.pageTextLayout(out, page: 1);
      expect(layout.text, page.layout!.text);

      final applied = '${dir.path}/applied.pdf';
      expect(PdfCore.applyOcr(scan, [page], applied), 1);
      expect((await PdfCore.ocrPage(applied, page: 1)).status, PdfOcrStatus.hasOcrLayer);
    } finally {
      await dir.delete(recursive: true);
    }
  }, skip: skip);
}
