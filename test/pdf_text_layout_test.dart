import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('glyph spans use UTF-16 offsets, including emoji and ligatures', () {
    final layout = PdfPageTextLayout.fromJson({
      'text': 'A😀fi',
      'width': 612,
      'height': 792,
      'glyphs': [
        {
          'start': 0,
          'end': 1,
          'bounds': [10, 20, 18, 32],
        },
        {
          'start': 1,
          'end': 3,
          'bounds': [18, 20, 30, 32],
        },
        {
          'start': 3,
          'end': 5,
          'bounds': [30, 20, 42, 32],
        },
      ],
    });
    expect(layout.width, 612.0);
    expect(layout.height, 792.0);
    expect(layout.hasText, true);
    expect(
      layout.glyphs.map(
        (glyph) => layout.text.substring(glyph.start, glyph.end),
      ),
      ['A', '😀', 'fi'],
    );
    expect(layout.glyphs[1].left, 18);
    expect(layout.glyphs[1].top, 20);
    expect(layout.glyphs[1].right, 30);
    expect(layout.glyphs[1].bottom, 32);
    expect(() => layout.glyphs.clear(), throwsUnsupportedError);
  });

  test('scanned/empty pages retain dimensions without selectable text', () {
    final layout = PdfPageTextLayout.fromJson({
      'text': '',
      'width': 792.5,
      'height': 612,
      'glyphs': [],
    });
    expect(layout.text, isEmpty);
    expect(layout.glyphs, isEmpty);
    expect(layout.hasText, false);
    expect(layout.width, 792.5);
  });

  test(
    'invalid offsets and geometry fail before reaching a selection overlay',
    () {
      for (final glyph in [
        {
          'start': -1,
          'end': 1,
          'bounds': [0, 0, 10, 10],
        },
        {
          'start': 0,
          'end': 2,
          'bounds': [0, 0, 10, 10],
        },
        {
          'start': 0,
          'end': 1,
          'bounds': [10, 0, 0, 10],
        },
        {
          'start': 0,
          'end': 1,
          'bounds': [0, double.nan, 10, 10],
        },
      ]) {
        expect(
          () => PdfPageTextLayout.fromJson({
            'text': 'A',
            'width': 612,
            'height': 792,
            'glyphs': [glyph],
          }),
          throwsFormatException,
        );
      }
      expect(
        () => PdfPageTextLayout.fromJson({
          'text': '',
          'width': 0,
          'height': 792,
          'glyphs': [],
        }),
        throwsFormatException,
      );
    },
  );

  test('page zero is rejected before resolving the native library', () async {
    await expectLater(
      PdfCore.pageTextLayout('/unused.pdf', page: 0),
      throwsRangeError,
    );
  });
}
