import 'package:plugin_platform_interface/plugin_platform_interface.dart';

import 'flutter_pdf_core_method_channel.dart';

abstract class FlutterPdfCorePlatform extends PlatformInterface {
  /// Constructs a FlutterPdfCorePlatform.
  FlutterPdfCorePlatform() : super(token: _token);

  static final Object _token = Object();

  static FlutterPdfCorePlatform _instance = MethodChannelFlutterPdfCore();

  /// The default instance of [FlutterPdfCorePlatform] to use.
  ///
  /// Defaults to [MethodChannelFlutterPdfCore].
  static FlutterPdfCorePlatform get instance => _instance;

  /// Platform-specific implementations should set this with their own
  /// platform-specific class that extends [FlutterPdfCorePlatform] when
  /// they register themselves.
  static set instance(FlutterPdfCorePlatform instance) {
    PlatformInterface.verifyToken(instance, _token);
    _instance = instance;
  }

  Future<String?> getPlatformVersion() {
    throw UnimplementedError('platformVersion() has not been implemented.');
  }
}
