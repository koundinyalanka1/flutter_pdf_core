// Stopping renders through the real native library, from Dart. Skipped unless
// PDF_CORE_LIB_PATH names a built library, for example:
//
//   PDF_CORE_LIB_PATH=rust/target/release/libpdf_ffi.so flutter test test/pdf_render_cancel_native_test.dart
import 'dart:convert';
import 'dart:io';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

/// A PDF of [objects], numbered from 1, with an exact cross-reference table,
/// so that nothing about it is recovered.
Uint8List pdfOf(List<String> objects) {
  final out = BytesBuilder();
  void write(String text) => out.add(latin1.encode(text));
  final offsets = <int>[];
  write('%PDF-1.7\n');
  for (var index = 0; index < objects.length; index++) {
    offsets.add(out.length);
    write('${index + 1} 0 obj\n${objects[index]}\nendobj\n');
  }
  final xref = out.length;
  write('xref\n0 ${objects.length + 1}\n0000000000 65535 f \n');
  for (final offset in offsets) {
    write('${offset.toString().padLeft(10, '0')} 00000 n \n');
  }
  write('trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\n'
      'startxref\n$xref\n%%EOF\n');
  return out.toBytes();
}

String streamObject(String dictionary, String content) =>
    '<< $dictionary /Length ${content.length} >>\nstream\n$content\nendstream';

/// A page that draws a form which draws itself four times, through the
/// nesting limit: millions of forms, each clipped to the whole page.
Uint8List formBomb() => pdfOf([
      '<< /Type /Catalog /Pages 2 0 R >>',
      '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
      '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 600] '
          '/Resources << /XObject << /X0 5 0 R >> >> /Contents 4 0 R >>',
      streamObject('', '/X0 Do'),
      streamObject(
        '/Type /XObject /Subtype /Form /BBox [0 0 600 600] '
            '/Resources << /XObject << /X0 5 0 R >> >>',
        '/X0 Do /X0 Do /X0 Do /X0 Do 0 0 1 1 re f',
      ),
    ]);

void main() {
  final library = Platform.environment['PDF_CORE_LIB_PATH'];
  final skip = library == null || library.isEmpty ? 'set PDF_CORE_LIB_PATH to run' : false;
  const sample = 'test/sample.pdf';
  late Directory dir;
  late String bomb;

  setUpAll(() {
    dir = Directory.systemTemp.createTempSync('pdf_render_cancel');
    bomb = '${dir.path}/bomb.pdf';
    File(bomb).writeAsBytesSync(formBomb());
  });

  tearDownAll(() {
    if (dir.existsSync()) dir.deleteSync(recursive: true);
  });

  test('another isolate cancels a running render by its id', () async {
    expect(PdfCore.canCancelRenders, isTrue);
    const renderId = 0x5EED0001;
    final path = bomb;
    final watch = Stopwatch()..start();
    final rendering = Isolate.run(() => PdfCore.renderPagePngWithWarnings(path, 0,
        width: 600, height: 600, renderId: renderId));
    await Future<void>.delayed(const Duration(milliseconds: 300));
    expect(PdfCore.cancelRender(renderId), isTrue, reason: 'the render was running');
    await expectLater(
      rendering,
      throwsA(isA<PdfException>()
          .having((e) => e.code, 'code', 'CANCELLED')
          .having((e) => e.isCancelled, 'isCancelled', isTrue)),
    );
    expect(watch.elapsed, lessThan(const Duration(seconds: 3)));
  }, skip: skip);

  test('a cancel that arrives before its render fails the render at once', () {
    const renderId = 0x5EED0002;
    expect(PdfCore.cancelRender(renderId), isFalse);
    final watch = Stopwatch()..start();
    expect(
      () => PdfCore.renderPagePngWithWarnings(bomb, 0,
          width: 600, height: 600, renderId: renderId),
      throwsA(isA<PdfException>().having((e) => e.code, 'code', 'CANCELLED')),
    );
    expect(watch.elapsed, lessThan(const Duration(seconds: 1)));
    // The cancel is used up: the same id renders the next page.
    expect(
      PdfCore.renderPagePngWithWarnings(sample, 0, width: 100, height: 100, renderId: renderId)
          .bytes,
      isNotEmpty,
    );
  }, skip: skip);

  test('an asynchronous render past its timeout is cancelled', () async {
    final watch = Stopwatch()..start();
    await expectLater(
      PdfCore.renderPagePngWithWarningsAsync(bomb, 0,
          width: 600, height: 600, timeout: const Duration(milliseconds: 500)),
      throwsA(isA<PdfException>().having((e) => e.code, 'code', 'TIMEOUT')),
    );
    expect(watch.elapsed, lessThan(const Duration(seconds: 2)));
    await expectLater(
      PdfCore.renderPageRegionRgbaWithWarningsAsync(bomb, 0,
          pageWidth: 600,
          pageHeight: 600,
          x: 0,
          y: 0,
          width: 300,
          height: 300,
          timeout: const Duration(milliseconds: 500)),
      throwsA(isA<PdfException>().having((e) => e.code, 'code', 'TIMEOUT')),
    );
  }, skip: skip);

  test('a page that renders in time is the same through every call', () async {
    final direct = PdfCore.renderPagePngWithWarnings(sample, 0, width: 300, height: 300);
    final viaId = PdfCore.renderPagePngWithWarnings(sample, 0,
        width: 300, height: 300, renderId: 0x5EED0003);
    final async = await PdfCore.renderPagePngWithWarningsAsync(sample, 0,
        width: 300, height: 300);
    expect(viaId.bytes, direct.bytes);
    expect(async.bytes, direct.bytes);
    expect(async.warnings, direct.warnings);
    final pixels = await PdfCore.renderPageRgbaAsync(sample, 0, width: 300, height: 300);
    final directPixels = PdfCore.renderPageRgba(sample, 0, width: 300, height: 300);
    expect((pixels.width, pixels.height), (directPixels.width, directPixels.height));
    expect(pixels.pixels, directPixels.pixels);
  }, skip: skip);
}
