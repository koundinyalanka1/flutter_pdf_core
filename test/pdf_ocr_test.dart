import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

Map<String, dynamic> _word(String text, double left, double right) => {
      'text': text,
      'confidence': 0.97,
      'bounds': [left, 100.0, right, 112.0],
      'quad': [
        [left, 100.0],
        [right, 100.0],
        [right, 112.0],
        [left, 112.0],
      ],
    };

Map<String, dynamic> _recognizedPage() => {
      'page': 2,
      'status': 'recognized',
      'width': 595.3,
      'height': 841.9,
      'dpi': 300.0,
      'skewDegrees': -0.4,
      'orientationDegrees': 180,
      'confidence': 0.97,
      'text': 'Invoice 42',
      'lines': [
        {
          'text': 'Invoice 42',
          'confidence': 0.97,
          'bounds': [72.0, 100.0, 140.0, 112.0],
          'words': [_word('Invoice', 72, 118), _word('42', 124, 140)],
        },
      ],
      'layout': {
        'text': 'Invoice 42',
        'width': 595.3,
        'height': 841.9,
        'glyphs': [
          {
            'start': 0,
            'end': 1,
            'bounds': [72.0, 100.0, 78.5, 112.0],
          },
        ],
      },
    };

void main() {
  test('a recognized page parses lines, words and selectable layout', () {
    final page = PdfOcrPage.fromJson(_recognizedPage());
    expect(page.page, 2);
    expect(page.status, PdfOcrStatus.recognized);
    expect(page.text, 'Invoice 42');
    expect(page.skewDegrees, -0.4);
    expect(page.orientationDegrees, 180);
    expect(page.lines.single.words.map((w) => w.text), ['Invoice', '42']);
    final word = page.lines.single.words.last;
    expect([word.left, word.top, word.right, word.bottom], [124, 100, 140, 112]);
    expect(word.quad, [124, 100, 140, 100, 140, 112, 124, 112]);
    expect(page.layout!.text, 'Invoice 42');
    expect(() => page.lines.clear(), throwsUnsupportedError);
  });

  test('pages that already have text are reported without geometry', () {
    final page = PdfOcrPage.fromJson({
      'page': 1,
      'status': 'hasOcrLayer',
      'text': '',
      'confidence': 0.0,
      'lines': [],
      'layout': null,
    });
    expect(page.status, PdfOcrStatus.hasOcrLayer);
    expect(page.layout, isNull);
    expect(page.lines, isEmpty);
  });

  test('results round-trip into what applyOcr sends to the engine', () {
    final page = PdfOcrPage.fromJson(_recognizedPage());
    final sent = page.toJson();
    expect(sent['page'], 2);
    final words = (sent['lines'] as List).single['words'] as List;
    expect(words.first['text'], 'Invoice');
    expect(words.first['quad'], [
      [72.0, 100.0],
      [118.0, 100.0],
      [118.0, 112.0],
      [72.0, 112.0],
    ]);
  });

  test('malformed results fail before reaching a viewer', () {
    for (final mutate in <void Function(Map<String, dynamic>)>[
      (json) => json['status'] = 'maybe',
      (json) => json['page'] = 0,
      (json) => (json['lines'] as List).first['bounds'] = [10, 10, 5, 5],
      (json) => ((json['lines'] as List).first['words'] as List).first['quad'] = [
            [0, 0],
          ],
      (json) => json['dpi'] = double.nan,
      (json) => json['orientationDegrees'] = 45,
    ]) {
      final json = _recognizedPage();
      mutate(json);
      expect(() => PdfOcrPage.fromJson(json), throwsFormatException);
    }
  });

  test('searchable-PDF reports list what happened to each page', () {
    final report = PdfSearchableReport.fromJson({
      'recognized': 1,
      'pages': [
        {'page': 1, 'status': 'hasText', 'words': 0, 'confidence': 0.0},
        {'page': 2, 'status': 'recognized', 'words': 210, 'confidence': 0.96},
      ],
    });
    expect(report.recognized, 1);
    expect(report.pages.map((p) => p.status), [PdfOcrStatus.hasText, PdfOcrStatus.recognized]);
    expect(report.pages.last.words, 210);
  });

  test('options serialize for the native engine', () {
    expect(const PdfOcrOptions(dpi: 200, force: true).toJson(), {
      'dpi': 200.0,
      'force': true,
      'minConfidence': 0.5,
      'deskew': true,
      'detectOrientation': true,
      'threads': 0,
    });
  });

  test('page zero is rejected before resolving the native library', () async {
    await expectLater(PdfCore.ocrPage('/missing.pdf', page: 0), throwsRangeError);
  });
}
