import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

// Pure-Dart tests (no native library needed). Full integration against the
// Rust core lives in `rust/` (cargo test) and example/integration_test.
void main() {
  group('PdfMetadata', () {
    test('round-trips through JSON', () {
      const meta = PdfMetadata(title: 'Report', author: 'Koundinya');
      final json = meta.toJson();
      expect(json['title'], 'Report');
      final back = PdfMetadata.fromJson(json);
      expect(back.title, 'Report');
      expect(back.author, 'Koundinya');
      expect(back.subject, isNull);
    });
  });

  group('PdfInfo', () {
    test('parses inspect JSON', () {
      final info = PdfInfo.fromJson({
        'version': '1.7',
        'encrypted': false,
        'objectCount': 12,
        'pageCount': 3,
        'metadata': {'title': 'T'},
      });
      expect(info.version, '1.7');
      expect(info.pageCount, 3);
      expect(info.metadata.title, 'T');
    });

    test('tolerates missing fields', () {
      final info = PdfInfo.fromJson(const {});
      expect(info.pageCount, 0);
      expect(info.encrypted, isFalse);
    });
  });

  group('PdfException', () {
    test('exposes machine-readable codes', () {
      final e = PdfException('WRONG_PASSWORD', 'incorrect password');
      expect(e.isWrongPassword, isTrue);
      expect(e.isEncrypted, isFalse);
      expect(e.toString(), contains('WRONG_PASSWORD'));
    });
  });
}
