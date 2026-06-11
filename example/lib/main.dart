import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:path_provider/path_provider.dart';

void main() {
  runApp(const MyApp());
}

/// Demonstrates the Rust-powered PDF toolkit: creates a tiny PDF on disk,
/// merges it with itself, rotates it, sets metadata, encrypts it, then
/// reads everything back through the FFI layer.
class MyApp extends StatefulWidget {
  const MyApp({super.key});

  @override
  State<MyApp> createState() => _MyAppState();
}

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

class _MyAppState extends State<MyApp> {
  String _log = 'Running demo…';

  @override
  void initState() {
    super.initState();
    _runDemo();
  }

  Future<void> _runDemo() async {
    final buffer = StringBuffer();
    try {
      buffer.writeln('Native core v${PdfCore.nativeVersion}');

      final dir = await getTemporaryDirectory();
      final input = '${dir.path}/demo.pdf';
      File(input).writeAsStringSync(_tinyPdf);
      buffer.writeln('pages(demo.pdf) = ${PdfCore.pageCount(input)}');

      final merged = '${dir.path}/merged.pdf';
      await PdfCore.mergeAsync([input, input, input], merged);
      buffer.writeln('merged ×3 → ${PdfCore.pageCount(merged)} pages');

      final rotated = '${dir.path}/rotated.pdf';
      await PdfCore.rotatePagesAsync(merged, 90, rotated);
      buffer.writeln('rotated 90°');

      final withMeta = '${dir.path}/meta.pdf';
      PdfCore.setMetadata(
          rotated, const PdfMetadata(title: 'FFI Demo'), withMeta);
      buffer.writeln('title = ${PdfCore.getMetadata(withMeta).title}');

      final encrypted = '${dir.path}/locked.pdf';
      await PdfCore.encryptAsync(withMeta, 'hunter2', encrypted);
      try {
        PdfCore.pageCount(encrypted);
      } on PdfException catch (e) {
        buffer.writeln('without password: ${e.code} ✓');
      }
      buffer.writeln(
          'with password: ${PdfCore.pageCount(encrypted, password: 'hunter2')} pages ✓');
    } catch (e) {
      buffer.writeln('FAILED: $e');
    }
    if (!mounted) return;
    setState(() => _log = buffer.toString());
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      home: Scaffold(
        appBar: AppBar(title: const Text('flutter_pdf_core example')),
        body: Padding(
          padding: const EdgeInsets.all(16),
          child: Text(_log, style: const TextStyle(fontFamily: 'monospace')),
        ),
      ),
    );
  }
}
