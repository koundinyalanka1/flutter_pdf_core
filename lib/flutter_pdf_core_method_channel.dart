import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'flutter_pdf_core_platform_interface.dart';

/// An implementation of [FlutterPdfCorePlatform] that uses method channels.
class MethodChannelFlutterPdfCore extends FlutterPdfCorePlatform {
  /// The method channel used to interact with the native platform.
  @visibleForTesting
  final methodChannel = const MethodChannel('flutter_pdf_core');

  @override
  Future<String?> getPlatformVersion() async {
    final version = await methodChannel.invokeMethod<String>(
      'getPlatformVersion',
    );
    return version;
  }
}
