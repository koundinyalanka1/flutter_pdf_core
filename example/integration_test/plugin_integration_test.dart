// End-to-end FFI smoke test: exercises the Rust core through dart:ffi on a
// real device/simulator (requires the native library to be bundled — see
// scripts/build_android.sh / build_ios.sh).
//
// Run with: flutter test integration_test

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:path_provider/path_provider.dart';

import 'package:flutter_pdf_core/flutter_pdf_core.dart';

// A minimal but complete one-page PDF (no compression).
const String _tinyPdf = '''%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>
endobj
xref
0 4
0000000000 65535 f
0000000009 00000 n
0000000058 00000 n
0000000115 00000 n
trailer
<< /Size 4 /Root 1 0 R >>
startxref
186
%%EOF
''';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  late Directory dir;
  late String input;

  setUpAll(() async {
    dir = await getTemporaryDirectory();
    input = '${dir.path}/in.pdf';
    File(input).writeAsStringSync(_tinyPdf);
  });

  testWidgets('native version is reachable', (tester) async {
    expect(PdfCore.nativeVersion.isNotEmpty, true);
  });

  testWidgets('inspect + page count', (tester) async {
    expect(PdfCore.pageCount(input), 1);
    final info = PdfCore.inspect(input);
    expect(info.pageCount, 1);
    expect(info.encrypted, false);
  });

  testWidgets('merge, rotate, metadata, encrypt round trip', (tester) async {
    final merged = '${dir.path}/merged.pdf';
    PdfCore.merge([input, input], merged);
    expect(PdfCore.pageCount(merged), 2);

    final rotated = '${dir.path}/rotated.pdf';
    PdfCore.rotatePages(merged, 90, rotated);
    expect(PdfCore.pageCount(rotated), 2);

    final withMeta = '${dir.path}/meta.pdf';
    PdfCore.setMetadata(
        rotated, const PdfMetadata(title: 'Integration'), withMeta);
    expect(PdfCore.getMetadata(withMeta).title, 'Integration');

    final encrypted = '${dir.path}/enc.pdf';
    PdfCore.encrypt(withMeta, 'pw-123', encrypted);
    expect(
      () => PdfCore.pageCount(encrypted),
      throwsA(isA<PdfException>().having((e) => e.isEncrypted, 'encrypted', true)),
    );
    expect(PdfCore.pageCount(encrypted, password: 'pw-123'), 2);

    final decrypted = '${dir.path}/dec.pdf';
    PdfCore.decrypt(encrypted, 'pw-123', decrypted);
    expect(PdfCore.pageCount(decrypted), 2);
  });

  testWidgets('split and AI export', (tester) async {
    final merged = '${dir.path}/m2.pdf';
    PdfCore.merge([input, input, input], merged);

    final firstTwo = '${dir.path}/first_two.pdf';
    PdfCore.extractPages(merged, '1-2', firstTwo);
    expect(PdfCore.pageCount(firstTwo), 2);

    final export = PdfCore.exportForAi(merged);
    expect(export, contains('flutter_pdf_core/export/v1'));
  });
}
