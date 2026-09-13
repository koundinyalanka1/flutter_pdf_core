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
  s.dependency 'Flutter'
  s.platform = :ios, '13.0'

  # Rust core (static XCFramework). Build it with `scripts/build_ios.sh`
  # from the package root before `pod install`. The -force_load flag keeps
  # the linker from stripping the FFI symbols that dart:ffi resolves at
  # runtime via DynamicLibrary.process().
  s.vendored_frameworks = 'Frameworks/PdfFfi.xcframework'

  # Flutter.framework does not contain a i386 slice.
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
    # -force_load points straight into the .xcframework rather than at the
    # copy CocoaPods extracts into PODS_XCFRAMEWORKS_BUILD_DIR.
    #
    # Xcode's build system treats a -force_load path as a declared build
    # input: it must either already exist or be the declared output of some
    # phase. For a static-library .xcframework CocoaPods writes the copy
    # phase's output filelist as "<pod>/PdfFfi.framework" while the file it
    # actually produces is "<pod>/libpdf_ffi.a", so nothing declares the .a
    # and the build fails with "Build input file cannot be found" before the
    # phase ever runs. These paths exist on disk beforehand, so they validate.
    'OTHER_LDFLAGS[sdk=iphoneos*]' => '$(inherited) -force_load "$(PODS_TARGET_SRCROOT)/Frameworks/PdfFfi.xcframework/ios-arm64/libpdf_ffi.a"',
    'OTHER_LDFLAGS[sdk=iphonesimulator*]' => '$(inherited) -force_load "$(PODS_TARGET_SRCROOT)/Frameworks/PdfFfi.xcframework/ios-arm64_x86_64-simulator/libpdf_ffi.a"',
  }
  s.swift_version = '5.0'

  # If your plugin requires a privacy manifest, for example if it uses any
  # required reason APIs, update the PrivacyInfo.xcprivacy file to describe your
  # plugin's privacy impact, and then uncomment this line. For more information,
  # see https://developer.apple.com/documentation/bundleresources/privacy_manifest_files
  # s.resource_bundles = {'flutter_pdf_core_privacy' => ['flutter_pdf_core/Sources/flutter_pdf_core/PrivacyInfo.xcprivacy']}
end
