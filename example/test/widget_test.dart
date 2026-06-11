// Basic widget smoke test. The FFI demo itself needs the native library,
// which isn't available in plain `flutter test`, so we only verify that the
// app builds and renders its scaffold (the demo reports errors in-app).

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:flutter_pdf_core_example/main.dart';

void main() {
  testWidgets('renders the demo scaffold', (WidgetTester tester) async {
    await tester.pumpWidget(const MyApp());
    expect(find.text('flutter_pdf_core example'), findsOneWidget);
  });
}
