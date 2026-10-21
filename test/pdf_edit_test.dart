// Filling, annotating and stamping. The JSON the engine is sent is checked
// here; the native round trip runs when PDF_CORE_LIB_PATH names a built
// library, for example:
//
//   PDF_CORE_LIB_PATH=rust/target/release/libpdf_ffi.so flutter test test/pdf_edit_test.dart
import 'dart:io';

import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('edits become the JSON the engine reads', () {
    final edits = PdfEdits(
      fields: const [
        PdfFieldValue.text('name', 'Ada'),
        PdfFieldValue.checked('agree', true),
        PdfFieldValue.selected('fruits', ['Apple', 'Cherry']),
      ],
      annotations: [
        PdfAnnotationEdit.markup(
          PdfAnnotationType.highlight,
          page: 1,
          rects: const [PdfBox(1, 2, 3, 4)],
          opacity: 0.5,
        ),
        PdfAnnotationEdit.ink(
          page: 2,
          strokes: const [
            [(1, 2), (3, 4)],
          ],
        ),
        PdfAnnotationEdit.textBox(
          page: 1,
          box: const PdfBox(0, 0, 50, 20),
          text: 'Hi',
          border: 0,
        ),
        PdfAnnotationEdit.note(page: 1, at: (5, 6), text: 'Note'),
        PdfAnnotationEdit.image(
          page: 1,
          box: const PdfBox(0, 0, 10, 10),
          image: const PdfImageSource.rgba('/tmp/x.rgba', 2, 3),
          flatten: true,
        ),
        PdfAnnotationEdit.line(page: 1, from: (0, 0), to: (9, 9), arrow: true),
      ],
      remove: const [PdfAnnotationRemoval(1, '12 0')],
      marks: const [
        PdfPageMark.pageNumbers(format: 'Page {page} of {pages}'),
        PdfPageMark.watermark('DRAFT', pages: [1]),
      ],
    );
    final json = edits.toJson();
    expect(json['fields'], [
      {'name': 'name', 'value': 'Ada'},
      {'name': 'agree', 'checked': true},
      {
        'name': 'fruits',
        'selected': ['Apple', 'Cherry'],
      },
    ]);
    final annotations = json['annotations'] as List;
    expect(annotations[0], containsPair('type', 'highlight'));
    expect(annotations[0], containsPair('color', '#FFEB3B'));
    expect(annotations[0]['rects'], [
      [1.0, 2.0, 3.0, 4.0],
    ]);
    expect(annotations[1]['strokes'], [
      [
        [1.0, 2.0],
        [3.0, 4.0],
      ],
    ]);
    expect(annotations[2]['type'], 'text', reason: 'no border: typed text');
    expect(annotations[3]['at'], [5.0, 6.0]);
    expect(annotations[4]['image'], {
      'path': '/tmp/x.rgba',
      'width': 2,
      'height': 3,
    });
    expect(annotations[4]['flatten'], isTrue);
    expect(annotations[5]['type'], 'arrow');
    expect(json['delete'], [
      {'page': 1, 'id': '12 0'},
    ]);
    final marks = json['marks'] as List;
    expect(marks[0]['text'], 'Page {page} of {pages}');
    expect(marks[1], containsPair('rotation', 45.0));
    expect(marks[1], containsPair('pages', [1]));
    expect(const PdfEdits().isEmpty, isTrue);
  });

  test('a form is read from the engine\'s JSON', () {
    final form = PdfForm.fromJson({
      'fields': [
        {
          'name': 'size',
          'kind': 'radio',
          'value': 'M',
          'states': ['S', 'M'],
          'options': [
            {'value': 'small', 'label': 'Small'},
          ],
          'widgets': [
            {
              'page': 1,
              'rect': [1, 2, 3, 4],
              'onState': 'S',
              'fontSize': 0,
            },
          ],
        },
      ],
      'xfa': true,
    });
    final field = form.fields.single;
    expect(field.kind, PdfFieldKind.radio);
    expect(field.isChecked, isTrue);
    expect(field.widgets.single.box, const PdfBox(1, 2, 3, 4));
    expect(field.options.single.label, 'Small');
    expect(form.xfa, isTrue);
    expect(form.signed, isFalse);
  });

  final library = Platform.environment['PDF_CORE_LIB_PATH'];
  final skip = library == null || library.isEmpty
      ? 'set PDF_CORE_LIB_PATH to run'
      : false;

  test('a note is added, listed and removed in place', () async {
    final dir = await Directory.systemTemp.createTemp('pdf_edit_test');
    addTearDown(() => dir.delete(recursive: true));
    final path = '${dir.path}/doc.pdf';
    final original = await File('test/sample.pdf').readAsBytes();
    await File(path).writeAsBytes(original);

    final warnings = await PdfCore.applyEditsAsync(
      path,
      PdfEdits(
        annotations: [
          PdfAnnotationEdit.note(page: 1, at: (20, 20), text: 'Hello ✓'),
        ],
        fields: const [PdfFieldValue.text('nothing', 'x')],
      ),
      path,
    );
    expect(warnings.single, contains('nothing'));
    final saved = await File(path).readAsBytes();
    expect(
      saved.sublist(0, original.length),
      original,
      reason: 'appended, not rewritten',
    );

    final notes = await PdfCore.annotationsAsync(path, 1);
    final note = notes.singleWhere((a) => a.ours);
    expect(note.subtype, 'Text');
    expect(note.contents, 'Hello ✓');
    expect(note.box, const PdfBox(20, 20, 44, 44));
    expect((await PdfCore.formFieldsAsync(path)).isEmpty, isTrue);

    await PdfCore.applyEditsAsync(
      path,
      PdfEdits(remove: [PdfAnnotationRemoval(1, note.id)]),
      path,
    );
    expect(
      (await PdfCore.annotationsAsync(path, 1)).where((a) => a.ours),
      isEmpty,
    );
  }, skip: skip);

  test('a smaller copy and pictures of the pages', () async {
    final dir = await Directory.systemTemp.createTemp('pdf_compress_test');
    addTearDown(() => dir.delete(recursive: true));
    final out = '${dir.path}/small.pdf';
    final report = await PdfCore.compressAsync(
      'test/sample.pdf',
      out,
      level: PdfCompressLevel.strong,
    );
    expect(report.bytesAfter, lessThanOrEqualTo(report.bytesBefore));
    expect(File(out).existsSync(), isTrue);
    final files = await PdfCore.pageImagesAsync(
      'test/sample.pdf',
      '${dir.path}/p-',
      format: PdfPageImageFormat.jpeg,
      dpi: 36,
    );
    expect(files, isNotEmpty);
    expect(files.first, endsWith('.jpg'));
    expect((await File(files.first).readAsBytes()).sublist(0, 2), [0xFF, 0xD8]);
  }, skip: skip);
}
