
import 'flutter_pdf_core_platform_interface.dart';

class FlutterPdfCore {
  Future<String?> getPlatformVersion() {
    return FlutterPdfCorePlatform.instance.getPlatformVersion();
  }
}
