#
# To learn more about a Podspec see http://guides.cocoapods.org/syntax/podspec.html.
# Run `pod lib lint flutter_pdf_core.podspec` to validate before publishing.
#
Pod::Spec.new do |s|
  s.name             = 'flutter_pdf_core'
  s.version          = '0.0.1'
  s.summary          = 'A new Flutter plugin project.'
  s.description      = <<-DESC
A new Flutter plugin project.
                       DESC
  s.homepage         = 'http://example.com'
  s.license          = { :file => '../LICENSE' }
  s.author           = { 'Your Company' => 'email@example.com' }

  s.source           = { :path => '.' }
  s.source_files = 'flutter_pdf_core/Sources/flutter_pdf_core/**/*'

  # If your plugin requires a privacy manifest, for example if it collects user
  # data, update the PrivacyInfo.xcprivacy file to describe your plugin's
  # privacy impact, and then uncomment this line. For more information,
  # see https://developer.apple.com/documentation/bundleresources/privacy_manifest_files
  # s.resource_bundles = {'flutter_pdf_core_privacy' => ['flutter_pdf_core/Sources/flutter_pdf_core/PrivacyInfo.xcprivacy']}

  s.dependency 'FlutterMacOS'

  # Rust core (universal dylib). Build it with `scripts/build_macos.sh`
  # from the package root before `pod install`.
  s.vendored_libraries = 'Frameworks/libpdf_ffi.dylib'

  s.platform = :osx, '10.14'
  s.pod_target_xcconfig = { 'DEFINES_MODULE' => 'YES' }
  s.swift_version = '5.0'
end
