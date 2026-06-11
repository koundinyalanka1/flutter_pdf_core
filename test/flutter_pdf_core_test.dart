import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_pdf_core/flutter_pdf_core.dart';
import 'package:flutter_pdf_core/flutter_pdf_core_platform_interface.dart';
import 'package:flutter_pdf_core/flutter_pdf_core_method_channel.dart';
import 'package:plugin_platform_interface/plugin_platform_interface.dart';

class MockFlutterPdfCorePlatform
    with MockPlatformInterfaceMixin
    implements FlutterPdfCorePlatform {
  @override
  Future<String?> getPlatformVersion() => Future.value('42');
}

void main() {
  final FlutterPdfCorePlatform initialPlatform = FlutterPdfCorePlatform.instance;

  test('$MethodChannelFlutterPdfCore is the default instance', () {
    expect(initialPlatform, isInstanceOf<MethodChannelFlutterPdfCore>());
  });

  test('getPlatformVersion', () async {
    FlutterPdfCore flutterPdfCorePlugin = FlutterPdfCore();
    MockFlutterPdfCorePlatform fakePlatform = MockFlutterPdfCorePlatform();
    FlutterPdfCorePlatform.instance = fakePlatform;

    expect(await flutterPdfCorePlugin.getPlatformVersion(), '42');
  });
}
